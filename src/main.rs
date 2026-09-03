mod constants;
use crate::constants::av::{
    AUDIO_NUM_ZEROES_CHECK_SIZE, AUDIO_PLAYING_STACK_SIZE, BOTTOM_WINDOW_WIDTH,
    MAX_PERMITTED_DATA_POLLS_PER_SECOND, OVERPOLL_COOLDOWN_MS, RGB_COLOR_SIZE, SCALING_FACTOR,
    TOP_WINDOW_WIDTH, USB_PROCESSING_STACK_SIZE, VIDEO_DISPLAY_EVENT_STACK_SIZE,
};
use clap::{Parser, Subcommand};
use constants::av::{
    AUDIO_BUFFER_SIZE, AUDIO_NUM_ZEROES_END_DELIMETER, AUDIO_SAMPLE_HZ, DEFAULT_TIMEOUT,
    FULL_BUFF_SIZE, PID_3DS, VEND_OUT_IDX, VEND_OUT_REQ, VEND_OUT_VALUE, VIDEO_BUFFER_SIZE,
    VID_3DS, WINDOW_HEIGHT, WINDOW_WIDTH,
};
use constants::av::{CANNOT_CONFIGURE_3DS, CANNOT_FIND_3DS, MAX_QUEUED_FRAMES};
use crossbeam::channel;
use dasp_sample::Sample;
use pixels::{Pixels, SurfaceTexture};
use rodio::Source;
use rusb::{DeviceHandle, GlobalContext};
use std::fs::File;
use std::io::Write;
use std::num::{NonZeroU16, NonZeroU32};
use std::ops::Sub;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Event, KeyEvent, WindowEvent};
use winit::event_loop::{EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window as WinitWindow};
use winit_input_helper::WinitInputHelper;

// Based on https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    // Follow-on command name
    name: Option<String>,

    // Path to configuration file
    // Not yet used.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    // For debug mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,

    #[arg(short, long)]
    split: bool,

    // Subcommands
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Dump {
        #[arg(short, long, value_name = "OUT_FILE", required(true))]
        outfile: Option<PathBuf>,
    },
}

struct DSConfig {
    using_kernel_driver: bool,
}

impl DSConfig {
    pub fn new(using_kernel_driver: bool) -> Self {
        Self {
            using_kernel_driver,
        }
    }
}

struct DS {
    config: DSConfig,
    handle: DeviceHandle<GlobalContext>,
    endpoint: Endpoint,
}

fn find_audio_frame_end(samples: &[i16]) -> usize {
    let frame_end = samples
        // Iterate over all contiguous windows of length 256
        .windows(AUDIO_NUM_ZEROES_END_DELIMETER)
        // Return point at which sample is just followed by constant num zeroes
        // The inference here is that it's unlikely an audio sample will be
        // completely silent unless it's the device indicating end-of-frame
        .position(|window| window.iter().all(|&x| x == 0))
        .unwrap_or(samples.len());

    // If sample has no terminating 0 set, return the full frame size.
    if frame_end >= AUDIO_NUM_ZEROES_END_DELIMETER {
        return frame_end;
    }

    // Handle the edge case where the sample size is < num zeroes
    // In that case we look for a smaller number of consecutive zeroes within the short sample
    // (e.g., 10).
    samples
        .windows(AUDIO_NUM_ZEROES_CHECK_SIZE)
        .position(|window| window.iter().all(|&x| x == 0))
        .unwrap_or(samples.len())
}

pub fn serve_audio(
    sink: &rodio::Player,
    audio_channel: &channel::Receiver<[u8; AUDIO_BUFFER_SIZE]>,
) {
    for audio in audio_channel {
        // Swap endianness
        let i16_sample: Vec<i16> = audio
            .chunks_exact(2)
            .map(|chunk| (chunk[1] as i16) << 8 | (chunk[0] as i16))
            .collect();

        let split_pt = find_audio_frame_end(&i16_sample);

        let remaining_sample: Vec<f32> = i16_sample[..split_pt]
            .iter()
            .map(|&samp| samp.to_sample::<f32>())
            .collect();

        let audio_src = rodio::buffer::SamplesBuffer::new(
            NonZeroU16::new(2).expect("Channel count cannot be zero"),
            NonZeroU32::new(AUDIO_SAMPLE_HZ).expect("Sample rate cannot be zero"),
            remaining_sample,
        )
        .speed(1.0);

        sink.append(audio_src);
    }
}

pub struct WindowUpdateEvent {
    video_buffer: Vec<u8>,
}

impl WindowUpdateEvent {
    fn new(video_buffer: Vec<u8>) -> Self {
        Self { video_buffer }
    }
}

pub fn serve_video(
    event_loop_proxy: &EventLoopProxy<WindowUpdateEvent>,
    video_channel: &channel::Receiver<[u8; VIDEO_BUFFER_SIZE]>,
) {
    for video in video_channel {
        // If we have exceeded the maximum queued frames, process and move on.
        if video_channel.len() > MAX_QUEUED_FRAMES {
            continue;
        }

        // We need a video sink here to track where vid is
        // and to ensure that video doesn't get too far behind

        let rotated_vid_buf = rotate_270(&video, WINDOW_HEIGHT, WINDOW_WIDTH);

        let video_data = WindowUpdateEvent::new(rotated_vid_buf);

        let _ = event_loop_proxy.send_event(video_data);
    }
}

impl DS {
    pub fn new(handle: DeviceHandle<GlobalContext>, endpoint: Endpoint) -> Self {
        let config = DSConfig::new(false);

        Self {
            config,
            handle,
            endpoint,
        }
    }

    pub fn configure(&mut self) -> Result<bool, anyhow::Error> {
        self.config.using_kernel_driver =
            match self.handle.kernel_driver_active(self.endpoint.iface) {
                Ok(true) => {
                    self.handle
                        .detach_kernel_driver(self.endpoint.iface)
                        .unwrap();
                    true
                }
                _ => false,
            };

        self.handle
            .set_active_configuration(self.endpoint.config)
            .unwrap();
        self.handle.claim_interface(self.endpoint.iface).unwrap();
        self.handle
            .set_alternate_setting(self.endpoint.iface, self.endpoint.setting)
            .unwrap();

        Ok(true)
    }

    pub fn write_control(&self) {
        let vend_out_buff = [0u8; 512];
        let vend_out_req_type = rusb::request_type(
            rusb::Direction::Out,
            rusb::RequestType::Vendor,
            rusb::Recipient::Device,
        );

        self.handle
            .write_control(
                vend_out_req_type,
                VEND_OUT_REQ,
                VEND_OUT_VALUE,
                VEND_OUT_IDX,
                &vend_out_buff,
                DEFAULT_TIMEOUT,
            )
            .expect("unable to vend out to device");
    }

    pub fn populate_buffers(
        &self,
        video_tx: &channel::Sender<[u8; VIDEO_BUFFER_SIZE]>,
        audio_tx: &channel::Sender<[u8; AUDIO_BUFFER_SIZE]>,
    ) {
        let mut buff = vec![0u8; FULL_BUFF_SIZE];

        let mut total_bytes_recd = 0;

        loop {
            match self.handle.read_bulk(
                self.endpoint.address,
                &mut buff[total_bytes_recd..],
                DEFAULT_TIMEOUT,
            ) {
                Ok(bytes_recd_this_time) => {
                    if bytes_recd_this_time == 0 {
                        break;
                    }

                    total_bytes_recd += bytes_recd_this_time
                }
                Err(err) => {
                    eprintln!("Unable to read from bulk endpoint: {}", err);
                    break;
                }
            }
        }

        // There is no need to populate the video and audio channels
        // If we did not retrieve any data.
        if total_bytes_recd == 0 {
            return;
        }

        let (vid_slice, audio_slice) = buff.split_at(VIDEO_BUFFER_SIZE);

        let mut vid_arr = [0u8; VIDEO_BUFFER_SIZE];
        vid_arr.copy_from_slice(vid_slice);

        let mut audio_arr = [0u8; AUDIO_BUFFER_SIZE];
        audio_arr.copy_from_slice(audio_slice);

        // Don't transmit more frames than we should store in the channel.
        // if video_tx.len() < MAX_QUEUED_FRAMES {
        video_tx.try_send(vid_arr).unwrap();
        // }

        if audio_tx.len() < MAX_QUEUED_FRAMES {
            audio_tx.try_send(audio_arr).unwrap()
        }
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    config: u8,
    iface: u8,
    setting: u8,
    address: u8,
}

impl Endpoint {
    pub fn new(config: u8, iface: u8, setting: u8, address: u8) -> Self {
        Self {
            config,
            iface,
            setting,
            address,
        }
    }
}

fn get_3ds_device() -> Result<DS, anyhow::Error> {
    let device = rusb::devices()
        .unwrap()
        .iter()
        .find(|dvc| {
            let desc = dvc.device_descriptor().unwrap();
            desc.vendor_id() == VID_3DS && desc.product_id() == PID_3DS
        })
        .ok_or(anyhow::Error::msg(
            "Expected device with VID 0x16D0, PID 0x06A3. Please reconnect your 3DS over USB.",
        ))
        .unwrap();

    let handle = rusb::open_device_with_vid_pid(VID_3DS, PID_3DS)
        .ok_or(anyhow::Error::msg("Could not retrieve device handle."))
        .unwrap();

    let config_desc = match device.config_descriptor(0) {
        Ok(cd) => cd,
        Err(e) => {
            return Err(anyhow::Error::msg(format!(
                "Could not fetch config descriptor: {}",
                e
            )))
        }
    };
    let interface = match config_desc.interfaces().last() {
        Some(iface) => iface,
        None => return Err(anyhow::Error::msg("Unable to retrieve interface.")),
    };
    let interface_desc = match interface.descriptors().last() {
        Some(id) => id,
        None => {
            return Err(anyhow::Error::msg(
                "Unable to retrieve inferface description.",
            ))
        }
    };
    let endpoint_desc = match interface_desc.endpoint_descriptors().last() {
        Some(ed) => ed,
        None => {
            return Err(anyhow::Error::msg(
                "Unable to retrieve endpoint description.",
            ))
        }
    };

    let endpoint = Endpoint::new(
        config_desc.number(),
        interface_desc.interface_number(),
        interface_desc.setting_number(),
        endpoint_desc.address(),
    );

    Ok(DS::new(handle, endpoint))
}

fn rotate_270(buffer: &[u8], width: usize, height: usize) -> Vec<u8> {
    // 3 values per pixel (R, G, B)
    let mut rotated_buffer = vec![0; width * height * 3];

    for y in 0..height {
        for x in 0..width {
            // Rotate 270 degrees (counterclockwise)
            let rotated_x = y;
            let rotated_y = width - 1 - x;

            // Map (x, y) from the original to the rotated position
            let old_px = (x + y * width) * 3;
            let new_px = (rotated_x + rotated_y * height) * 3;

            rotated_buffer[new_px] = buffer[old_px];
            rotated_buffer[new_px + 1] = buffer[old_px + 1];
            rotated_buffer[new_px + 2] = buffer[old_px + 2];
        }
    }

    rotated_buffer
}

struct Counter {
    name: String,
    start_time: SystemTime,
    current_frames: i32,
}

impl Counter {
    pub fn new(name: String) -> Self {
        Self {
            name,
            start_time: std::time::SystemTime::now(),
            current_frames: 0,
        }
    }

    pub fn maybe_print_fps(&mut self) {
        let current_time = std::time::SystemTime::now();

        let one_second_ago = current_time.sub(std::time::Duration::from_secs(1));
        if one_second_ago.gt(&self.start_time) {
            self.start_time = current_time;
            eprintln!("{} frames/second: {}", self.name, self.current_frames);
            self.current_frames = 0;
        }

        self.increment_frame();
    }

    pub fn increment_frame(&mut self) {
        self.current_frames += 1;
    }
}

fn main() {
    let cli = Cli::parse();

    // Configure the 3DS
    let mut ds = get_3ds_device().expect(CANNOT_FIND_3DS);
    ds.configure().expect(CANNOT_CONFIGURE_3DS);

    // Create audio output stream
    let stream_handle = rodio::DeviceSinkBuilder::open_default_sink().unwrap();
    let player = rodio::Player::connect_new(stream_handle.mixer());

    // Maintain a counter of frames retrieved over USB per second
    let mut usb_polls_per_second = Counter::new("USB".to_string());

    // Initialize the winit event loop.
    let event_loop = EventLoop::<WindowUpdateEvent>::with_user_event()
        .build()
        .unwrap();

    // Proxy is needed to transmit video data to the window.
    let evt_loop_proxy = event_loop.create_proxy();

    // Simplifies accessing keyboard and mouse inputs.
    let mut input = WinitInputHelper::new();

    // Create channels for video and audio.
    let (video_tx, video_rx): (
        channel::Sender<[u8; VIDEO_BUFFER_SIZE]>,
        channel::Receiver<[u8; VIDEO_BUFFER_SIZE]>,
    ) = channel::bounded(MAX_QUEUED_FRAMES);
    let (audio_tx, audio_rx): (
        channel::Sender<[u8; AUDIO_BUFFER_SIZE]>,
        channel::Receiver<[u8; AUDIO_BUFFER_SIZE]>,
    ) = channel::bounded(MAX_QUEUED_FRAMES);

    // Spawn thread to fill buffers with video and audio data.
    // Constantly retrieve new data from USB.
    std::thread::Builder::new()
        .stack_size(USB_PROCESSING_STACK_SIZE)
        .spawn(move || loop {
            ds.write_control();
            ds.populate_buffers(&video_tx, &audio_tx);
            if cli.debug > 0 {
                usb_polls_per_second.maybe_print_fps();
            }

            // This is important to ensure that our client does not try
            // to poll data from the device when it is closed. Without a cooldown
            // we can make over 1000 attempts/second, which exceeds max fps (60).
            if usb_polls_per_second.current_frames > MAX_PERMITTED_DATA_POLLS_PER_SECOND {
                std::thread::sleep(Duration::from_millis(OVERPOLL_COOLDOWN_MS));
            }
        })
        .unwrap();

    // The absence of a loop can cause audio stutter when the sink is empty.
    std::thread::Builder::new()
        .stack_size(AUDIO_PLAYING_STACK_SIZE)
        .spawn(move || loop {
            serve_audio(&player, &audio_rx);
        })
        .unwrap();

    std::thread::Builder::new()
        .stack_size(VIDEO_DISPLAY_EVENT_STACK_SIZE)
        .spawn(move || loop {
            serve_video(&evt_loop_proxy, &video_rx);
        })
        .unwrap();

    let main_window_width = match cli.split {
        true => TOP_WINDOW_WIDTH as f64,
        false => WINDOW_WIDTH as f64,
    };

    // Create a basic window.
    // Guidance from https://github.com/parasyte/pixels/tree/main/examples/conway
    let winit_main_window = {
        let size = LogicalSize::new(main_window_width, WINDOW_HEIGHT as f64);
        let scaled_size = LogicalSize::new(
            main_window_width * SCALING_FACTOR,
            WINDOW_HEIGHT as f64 * SCALING_FACTOR,
        );

        // TODO - Restructure to use the new 'app' interface.
        #[allow(deprecated)]
        Arc::new(
            event_loop
                .create_window(
                    WinitWindow::default_attributes()
                        .with_title("OxiDS")
                        .with_inner_size(scaled_size)
                        .with_min_inner_size(size),
                )
                .unwrap(),
        )
    };

    let winit_secondary_window = match cli.split {
        true => Some({
            let size = LogicalSize::new(320.0, 240.0);
            let scaled_size = LogicalSize::new(320.0 * SCALING_FACTOR, 240.0 * SCALING_FACTOR);

            // TODO - Restructure to use the new 'app' interface.
            #[allow(deprecated)]
            Arc::new(
                event_loop
                    .create_window(
                        WinitWindow::default_attributes()
                            .with_title("OxiDS (Bottom Screen)")
                            .with_inner_size(scaled_size)
                            .with_min_inner_size(size),
                    )
                    .unwrap(),
            )
        }),
        false => None,
    };

    // Use default window size for the pixels interface.
    let mut pixels = {
        let window_size = winit_main_window.inner_size();
        let surface_texture =
            SurfaceTexture::new(window_size.width, window_size.height, &winit_main_window);
        Pixels::new(
            main_window_width as u32,
            WINDOW_HEIGHT as u32,
            surface_texture,
        )
        .unwrap()
    };

    let mut pixels_secondary = match cli.split {
        // Unwraps are safe because the secondary window will exist if this config option is enabled.
        true => Some({
            let winit_secondary_window_unwrapped = winit_secondary_window.as_ref().unwrap();
            let window_size = winit_secondary_window_unwrapped.inner_size();
            let surface_texture = SurfaceTexture::new(
                window_size.width,
                window_size.height,
                winit_secondary_window_unwrapped,
            );
            Pixels::new(320, 240, surface_texture).unwrap()
        }),
        false => None,
    };

    // Print debug information to confirm GPU is being used.
    if cli.debug > 0 {
        let info = pixels.adapter().get_info();
        eprintln!("Using the following GPU for rendering: {:?}", info.name);
    }

    // Once per run, decide if there is a need to dump video output.
    // Error out if no outfile was provided or the filename matches
    // a file that already exists, to avoid accidental overwriting of data.
    let mut outfile_name = match &cli.command {
        Some(Commands::Dump { outfile }) => {
            let output = outfile.as_deref().expect("Error: No outfile provided.");

            let out_as_file = File::create_new(output).expect(
                "Error: File would be overwritten. Specify a file name that does not yet exist.",
            );

            Some(out_as_file)
        }
        None => None,
    };

    #[allow(deprecated)]
    let _ = event_loop.run(|event, _elwt| match event {
        Event::Resumed => {}
        Event::NewEvents(_) => {
            input.step();
        }
        Event::DeviceEvent { event, .. } => {
            input.process_device_event(&event);
        }
        Event::UserEvent(e) => {
            let WindowUpdateEvent { video_buffer } = e;
            // Whenever we get an event, replace all pixels in buffer
            // With the new image
            let mut counter = 0;
            let mut top_line_counter = 0;

            let mut_px = pixels.frame_mut().chunks_mut(4);
            let mut_secondary_px = match cli.split {
                true => Some(pixels_secondary.as_mut().unwrap().frame_mut().chunks_mut(4)),
                false => None,
            };

            // Render the top screen
            for pixel in mut_px {
                // R, G, B
                pixel[0] = video_buffer[counter];
                pixel[1] = video_buffer[counter + 1];
                pixel[2] = video_buffer[counter + 2];

                // The capture card doesn't appear to transmit alpha values
                // so we hardcode the 4th value, which is alpha/opacity to 100%.
                pixel[3] = 255;

                // Increment video_buffer counter by 3 (not 4) since it omits
                // alpha values.
                counter += 3;
                top_line_counter += 1;

                // Once we have rendered 400 pixels on the line in split
                // we must skip the next 320 * 3 (RGB).
                if cli.split && top_line_counter % TOP_WINDOW_WIDTH == 0 {
                    top_line_counter = 0;
                    counter += BOTTOM_WINDOW_WIDTH * RGB_COLOR_SIZE;
                }
            }

            if cli.split {
                // If a second window is being rendered, we need to offset,
                // and not render, the first 400 * 3 bytes of data for each line,
                // which represents the main screen.
                let mut line_counter = TOP_WINDOW_WIDTH * RGB_COLOR_SIZE;
                for pixel in mut_secondary_px.unwrap() {
                    // R, G, B
                    pixel[0] = video_buffer[line_counter];
                    pixel[1] = video_buffer[line_counter + 1];
                    pixel[2] = video_buffer[line_counter + 2];

                    // The capture card doesn't appear to transmit alpha values
                    // so we hardcode the 4th value, which is alpha/opacity to 100%.
                    pixel[3] = 255;

                    // Iterate to the next pixel
                    line_counter += 3;

                    // But, skip the first 400 pixels (main screen)
                    // if we've reached the end of the line
                    if line_counter.is_multiple_of(WINDOW_WIDTH * RGB_COLOR_SIZE) {
                        line_counter += TOP_WINDOW_WIDTH * RGB_COLOR_SIZE;
                    }
                }

                pixels_secondary.as_mut().unwrap().render().unwrap();
            }

            // If specified an outfile, dump pixel buffer to a file.
            if let Some(f) = outfile_name.as_mut() {
                f.write_all(pixels.frame()).expect("FAILED TO WRITE DATA");
            }

            // TODO -> handle error
            pixels.render().unwrap();
        }
        // TODO -> Custom handling for some event types.
        Event::Suspended => {}
        Event::AboutToWait => {}
        Event::LoopExiting => {}
        Event::MemoryWarning => {}
        Event::WindowEvent { event, .. } => {
            // Matching paradigm based on https://docs.rs/winit/latest/winit/event/struct.KeyEvent.html
            match event {
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            physical_key: PhysicalKey::Code(KeyCode::Enter),
                            state: ElementState::Pressed,
                            repeat: false,
                            ..
                        },
                    ..
                } => {
                    // OSX offers .set_simple_fullscreen(), but other platforms do not.
                    // For now, this will be implemented with set_fullscreen
                    // for platform independence.
                    winit_main_window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                }
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            physical_key: PhysicalKey::Code(KeyCode::Escape),
                            state: ElementState::Pressed,
                            repeat: false,
                            ..
                        },
                    ..
                } => {
                    winit_main_window.set_fullscreen(None);
                }

                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            physical_key: PhysicalKey::Code(KeyCode::Backspace),
                            state: ElementState::Pressed,
                            repeat: false,
                            ..
                        },
                    ..
                } => {
                    let _ = winit_main_window.request_inner_size(LogicalSize {
                        width: WINDOW_WIDTH as f64 * SCALING_FACTOR,
                        height: WINDOW_HEIGHT as f64 * SCALING_FACTOR,
                    });
                }
                _ => {
                    // Any other keys to be handled can go here.
                }
            }
        }
    });

    // TODO - cleanly release interface when program closes.
    // ds.handle.release_interface(ds.endpoint.iface).unwrap();
    // if ds.config.using_kernel_driver {
    //     ds.handle.attach_kernel_driver(&ds.endpoint.iface).unwrap();
    // };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_buffers() {
        let initial_buff: &[u8] = &[
            255, 0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 100, 90, 80, 70, 60, 50, 40, 30, 20,
            10, 0, 255,
        ];

        // Output buffer size is 3x width * height value since each has RGB values.
        let rotated_buff = rotate_270(initial_buff, 2, 4);

        let result: &[u8] = &[
            20, 30, 40, 80, 90, 100, 70, 60, 50, 10, 0, 255, 255, 0, 10, 50, 60, 70, 100, 90, 80,
            40, 30, 20,
        ];

        assert_eq!(*rotated_buff, *result);
    }

    #[test]
    fn finds_audio_frame_end_with_short_sample() {
        // With a very short sample we should still be able to discern the cutoff
        let test_buffer: &[i16] = &[
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];

        assert_eq!(find_audio_frame_end(test_buffer), 10);
    }

    #[test]
    fn finds_audio_frame_end_with_long_sample() {
        // With a very short sample we should still be able to discern the cutoff
        let test_buffer: &[i16] = &[
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858,
            494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235,
            255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024,
            843, 2930, 235, 255, 939, 293, 858, 494, 999, 1024, 843, 2930, 235, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];

        assert_eq!(find_audio_frame_end(test_buffer), 330);
    }
}
