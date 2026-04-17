mod constants;
use constants::av::{
    AUDIO_BUFFER_SIZE, AUDIO_NUM_ZEROES_END_DELIMETER, AUDIO_SAMPLE_HZ, AUDIO_THREAD_STACK_SIZE,
    DEFAULT_TIMEOUT, FULL_BUFF_SIZE, PID_3DS, TARGET_FPS, VEND_OUT_IDX, VEND_OUT_REQ,
    VEND_OUT_VALUE, VIDEO_BUFFER_SIZE, VIDEO_THREAD_STACK_SIZE, VID_3DS, WINDOW_HEIGHT,
    WINDOW_WIDTH,
};
use constants::av::{CANNOT_CONFIGURE_3DS, CANNOT_FIND_3DS, MAX_QUEUED_FRAMES};
use crossbeam::channel;
use minifb::Scale;
use minifb::ScaleMode;
use minifb::Window;
use minifb::WindowOptions;
use rodio::{OutputStream, Source};
use rusb::{DeviceHandle, GlobalContext};
use std::ops::Sub;
use std::time::SystemTime;

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
    // A long sequence of zeroes indicates the end of the audio frame
    samples
        .windows(AUDIO_NUM_ZEROES_END_DELIMETER)
        .position(|window| window.iter().all(|&x| x == 0))
        .unwrap_or(samples.len())
}

pub fn serve_audio(sink: &rodio::Sink, audio_channel: &channel::Receiver<[u8; AUDIO_BUFFER_SIZE]>) {
    for audio in audio_channel {
        // Swap endianness
        let i16_sample: Vec<i16> = audio
            .chunks_exact(2)
            .map(|chunk| (chunk[1] as i16) << 8 | (chunk[0] as i16))
            .collect();

        let split_pt = find_audio_frame_end(&i16_sample);

        let remaining_sample = &i16_sample[..split_pt];

        let audio_src =
            rodio::buffer::SamplesBuffer::new(2, AUDIO_SAMPLE_HZ, remaining_sample).speed(1.0);

        sink.append(audio_src);
    }
}

pub fn serve_video(
    window: &mut Window,
    video_channel: &channel::Receiver<[u8; VIDEO_BUFFER_SIZE]>,
) {
    for video in video_channel {
        // We need a video sink here to track where vid is
        // and to ensure that video doesn't get togggar behind
        let vid_buf_32 = u8_to_u32(&video);
        let rotated_vid_buf = rotate_270(&vid_buf_32, WINDOW_HEIGHT, WINDOW_WIDTH);
        window
            .update_with_buffer(&rotated_vid_buf, WINDOW_WIDTH, WINDOW_HEIGHT)
            .unwrap();
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

        loop {
            match self
                .handle
                .read_bulk(self.endpoint.address, &mut buff, DEFAULT_TIMEOUT)
            {
                Ok(bytes_rec) => {
                    if bytes_rec == 0 {
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("unable to read from bulk endpoint: {}", err);
                }
            }
        }

        let (vid_slice, audio_slice) = buff.split_at(VIDEO_BUFFER_SIZE);

        let mut vid_arr = [0u8; VIDEO_BUFFER_SIZE];
        vid_arr.copy_from_slice(vid_slice);

        let mut audio_arr = [0u8; AUDIO_BUFFER_SIZE];
        audio_arr.copy_from_slice(audio_slice);

        if video_tx.len() < MAX_QUEUED_FRAMES {
            video_tx.try_send(vid_arr).unwrap();
        }

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

struct CustomWindowOptions {
    opts: WindowOptions,
}

impl CustomWindowOptions {
    pub fn new(borderless: bool, resize: bool, scale: Scale, scale_mode: ScaleMode) -> Self {
        Self {
            opts: WindowOptions {
                borderless,
                resize,
                scale,
                scale_mode,
                none: false,
                title: true,
                topmost: false,
                transparency: false,
            },
        }
    }

    pub fn inner(&self) -> WindowOptions {
        self.opts
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
        .ok_or(anyhow::Error::msg("unable to find 3ds device"))
        .unwrap();

    let handle = rusb::open_device_with_vid_pid(VID_3DS, PID_3DS)
        .ok_or(anyhow::Error::msg("unable to retrieve device handle"))
        .unwrap();

    let config_desc = match device.config_descriptor(0) {
        Ok(cd) => cd,
        Err(e) => {
            return Err(anyhow::Error::msg(format!(
                "unable to get config descriptor: {}",
                e
            )))
        }
    };
    let interface = match config_desc.interfaces().last() {
        Some(iface) => iface,
        None => return Err(anyhow::Error::msg("unable to retrieve interface")),
    };
    let interface_desc = match interface.descriptors().last() {
        Some(id) => id,
        None => {
            return Err(anyhow::Error::msg(
                "unable to retrieve inferface description",
            ))
        }
    };
    let endpoint_desc = match interface_desc.endpoint_descriptors().last() {
        Some(ed) => ed,
        None => {
            return Err(anyhow::Error::msg(
                "unable to retrieve endpoint description",
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

fn rotate_270(buffer: &[u32], width: usize, height: usize) -> Vec<u32> {
    let mut rotated_buffer = vec![0; width * height];

    for y in 0..height {
        for x in 0..width {
            // Rotate 270 degrees (counterclockwise)
            let rotated_x = y;
            let rotated_y = width - 1 - x;

            // Map (x, y) from the original to the rotated position
            rotated_buffer[rotated_x + rotated_y * height] = buffer[x + y * width];
        }
    }

    rotated_buffer
}

fn u8_to_u32(u8_buffer: &[u8]) -> Vec<u32> {
    let mut u32_buffer = Vec::with_capacity(u8_buffer.len() / 3);
    // See if we can replace with chunks exact?
    for chunk in u8_buffer.chunks(3) {
        if chunk.len() == 3 {
            let r = chunk[0] as u32;
            let g = chunk[1] as u32;
            let b = chunk[2] as u32;
            let alpha = 255;

            let px = (alpha << 24) | (r << 16) | (g << 8) | b;

            u32_buffer.push(px);
        } else {
            println!("chunk not complete");
            println!("{:?}", chunk);
        }
    }

    u32_buffer
}

struct FpsCounter {
    start_time: SystemTime,
    current_frames: i32,
}

impl FpsCounter {
    pub fn new() -> Self {
        Self {
            start_time: std::time::SystemTime::now(),
            current_frames: 0,
        }
    }

    pub fn maybe_print_usb_dataps(&mut self) {
        let current_time = std::time::SystemTime::now();

        let one_second_ago = current_time.sub(std::time::Duration::from_secs(1));
        if one_second_ago.gt(&self.start_time) {
            self.start_time = current_time;
            println!("Data frames/second: {}", self.current_frames);
            self.current_frames = 0;
        }
    }

    pub fn increment_frame(&mut self) {
        self.current_frames += 1;
    }
}

fn main() {
    let mut ds = get_3ds_device().expect(CANNOT_FIND_3DS);
    ds.configure().expect(CANNOT_CONFIGURE_3DS);

    // Create audio output stream
    let (_audio_str, audio_stream_handle) =
        OutputStream::try_default().expect("couldnt create output stream");
    let sink = rodio::Sink::try_new(&audio_stream_handle).unwrap();

    // Start Window
    let opts = CustomWindowOptions::new(true, true, Scale::X2, ScaleMode::AspectRatioStretch);
    let mut window =
        minifb::Window::new("OxiDS", WINDOW_WIDTH, WINDOW_HEIGHT, opts.inner()).unwrap();
    window.set_target_fps(TARGET_FPS);

    // Start FPS Counter
    let mut counter = FpsCounter::new();

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
    std::thread::Builder::new()
        .stack_size(VIDEO_THREAD_STACK_SIZE)
        .spawn(move || loop {
            ds.write_control();
            ds.populate_buffers(&video_tx, &audio_tx);
            counter.maybe_print_usb_dataps();
            counter.increment_frame();
        })
        .unwrap();

    // Spawn thread to serve audio using the sink.
    std::thread::Builder::new()
        .stack_size(AUDIO_THREAD_STACK_SIZE)
        .spawn(move || loop {
            serve_audio(&sink, &audio_rx);
        })
        .unwrap();

    // As long as window is open, serve video in main thread.
    while window.is_open() && !window.is_key_down(minifb::Key::Escape) {
        serve_video(&mut window, &video_rx);
    }

    // Handle separately
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
        let initial_buff: &[u32] = &[255, 0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

        let rotated_buff = rotate_270(initial_buff, 3, 4);

        let result: &[u32] = &[10, 40, 70, 100, 0, 30, 60, 90, 255, 20, 50, 80];

        assert_eq!(*rotated_buff, *result);
    }
}
