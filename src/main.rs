// TODO
// Consider using formal audio codecs - see https://docs.rs/symphonia/latest/symphonia/all.html#structs
use minifb::Scale;
use minifb::ScaleMode;
use minifb::Window;
use minifb::WindowOptions;
use rodio::OutputStream;
use rusb::{DeviceHandle, GlobalContext};
use std::ops::Sub;
use std::time::Duration;
use std::time::SystemTime;

// const BULK_ENDPOINT_ADDRESS: u8 = 130;
const VID_3DS: u16 = 0x16D0;
const PID_3DS: u16 = 0x06A3;

// Will this break if we drop below 10fps?
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
const VEND_OUT_REQ: u8 = 0x40;
const VEND_OUT_VALUE: u16 = 0;
const VEND_OUT_IDX: u16 = 0;

const VIDEO_WIDTH: usize = 240;
const VIDEO_HEIGHT: usize = 720;
const RGB_COLOR_SIZE: usize = 3;
const VIDEO_BUFFER_SIZE: usize = VIDEO_WIDTH * VIDEO_HEIGHT * RGB_COLOR_SIZE;

// const UNKNOWN_BUFFER_SIZE: usize = 1916;
// const UNKNOWN_BUFFER_SIZE: usize = 2000;
// I think this is the solution. Just need to get the right number here.

// 1028 (2 ^ 10) * 4 - 2 (2^ 12).
// const AUDIO_BUFFER_SIZE: usize = 4112;
// BELOW WAS THE CONFIRMDD CORRECT ONE
const AUDIO_BUFFER_SIZE: usize = 4376;
const AUDIO_SAMPLE_HZ: u32 = 32728;
const MAX_PERMITTED_AUDIO_FRAME_DELAY: usize = 5;
// const AUDIO_TRUNCATED_BYTES: usize = 6;
// const AUDIO_TRUNCATED_BYTES: usize = 4;
// const AUDIO_SAMPLE_HZ: u32 = (32728.0 * GAP_MULTIPLIER) as u32;

const FULL_BUFF_SIZE: usize = VIDEO_BUFFER_SIZE + AUDIO_BUFFER_SIZE;
// This is just an initial value when window can be resized.
const WINDOW_HEIGHT: usize = 240;
const WINDOW_WIDTH: usize = 720;

// Not reaching 60 fps - seems locked at 30.
const TARGET_FPS: usize = 60;
struct DS {
    handle: DeviceHandle<GlobalContext>,
    endpoint: Endpoint,
    using_kernel_driver: bool,
    display: Display,
}

struct Display {
    window: Window,
}

impl Display {
    pub fn new(window: Window) -> Self {
        Self { window }
    }

    pub fn serve_video(&mut self, video: [u8; VIDEO_BUFFER_SIZE]) {
        let vid_buf_32 = u8_to_u32(&video);
        let rotated_vid_buf = rotate_270(&vid_buf_32, WINDOW_HEIGHT, WINDOW_WIDTH);
        self.window
            .update_with_buffer(&rotated_vid_buf, WINDOW_WIDTH, WINDOW_HEIGHT)
            .unwrap();
    }
}

impl DS {
    pub fn new(handle: DeviceHandle<GlobalContext>, endpoint: Endpoint) -> Self {
        let opts = CustomWindowOptions::new(true, true, Scale::X2, ScaleMode::AspectRatioStretch);

        let mut window =
            minifb::Window::new("Krab3DS", WINDOW_WIDTH, WINDOW_HEIGHT, opts.inner()).unwrap();
        window.set_target_fps(TARGET_FPS);

        let display = Display::new(window);

        Self {
            handle,
            using_kernel_driver: false,
            endpoint,
            display,
        }
    }

    pub fn configure(&mut self) -> Result<bool, anyhow::Error> {
        self.using_kernel_driver = match self.handle.kernel_driver_active(self.endpoint.iface) {
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

    // Should try moving this to a separate audio device
    pub fn serve_audio(&self, sink: &rodio::Sink, audio: [u8; AUDIO_BUFFER_SIZE]) {
        let i16_sample: Vec<i16> = audio
            .chunks(2)
            .map(|chunk| (chunk[1] as i16) << 8 | (chunk[0] as i16))
            .collect();

        // The last 4 bytes in the buffer EXCEPT IN RARE CASES don't contain any data.
        // So we must size the array to include it, but can truncate.
        // Think we can slow down speed but in doing so need to change hz
        let (remaining_sample, _truncated) = i16_sample.split_at(AUDIO_BUFFER_SIZE / 2);

        // Set speed appropriately - might not ultimately be necessary.
        let audio_src = rodio::buffer::SamplesBuffer::new(2, AUDIO_SAMPLE_HZ, remaining_sample);

        // If our audio starts getting behind due to hardware lag, reset before adding the next sound
        // to the sink.
        if sink.len() > MAX_PERMITTED_AUDIO_FRAME_DELAY {
            sink.clear();
            sink.play();
        }

        sink.append(audio_src);
    }

    pub fn get_buffers(&self) -> ([u8; VIDEO_BUFFER_SIZE], [u8; AUDIO_BUFFER_SIZE]) {
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
                    break;
                }
            }
        }

        let (vid_slice, audio_slice) = buff.split_at(VIDEO_BUFFER_SIZE);
        let mut vid_arr = [0u8; VIDEO_BUFFER_SIZE];
        vid_arr.copy_from_slice(vid_slice);

        let mut audio_arr = [0u8; AUDIO_BUFFER_SIZE];
        audio_arr.copy_from_slice(audio_slice);
        (vid_arr, audio_arr)
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

    // Should this be here or trait impl?
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

// CHUNKING CODE
fn u8_to_u32(u8_buffer: &[u8]) -> Vec<u32> {
    let mut u32_buffer = Vec::with_capacity(u8_buffer.len() / 3);
    for chunk in u8_buffer.chunks(3) {
        if chunk.len() == 3 {
            let r = chunk[0] as u32;
            let g = chunk[1] as u32;
            let b = chunk[2] as u32;
            let alpha = 255; // Code max opacity

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

    pub fn maybe_print_fps(&mut self) {
        let current_time = std::time::SystemTime::now();

        let one_second_ago = current_time.sub(std::time::Duration::from_secs(1));
        if one_second_ago.gt(&self.start_time) {
            self.start_time = current_time;
            println!("FPS: {}", self.current_frames);
            self.current_frames = 0;
        }
    }

    pub fn increment_frame(&mut self) {
        self.current_frames += 1;
    }
}

fn main() {
    let mut ds = get_3ds_device().expect("unable to locate 3ds device");
    ds.configure().expect("could not configure 3ds");

    // Start Audio
    let (_audio_str, audio_stream_handle) =
        OutputStream::try_default().expect("couldnt create output stream");
    let sink = rodio::Sink::try_new(&audio_stream_handle).unwrap();

    // Run
    let mut counter = FpsCounter::new();
    while ds.display.window.is_open() && !ds.display.window.is_key_down(minifb::Key::Escape) {
        ds.write_control();
        let (video, audio) = ds.get_buffers();
        ds.serve_audio(&sink, audio);
        ds.display.serve_video(video);
        counter.maybe_print_fps();
        counter.increment_frame();
    }

    // Release interface
    ds.handle.release_interface(ds.endpoint.iface).unwrap();
    if ds.using_kernel_driver {
        ds.handle.attach_kernel_driver(ds.endpoint.iface).unwrap();
    };
}
