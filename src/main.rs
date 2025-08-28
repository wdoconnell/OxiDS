mod constants;
use constants::av::MAX_PERMITTED_AUDIO_FRAME_SAMPLE_DELAY_NUM;
use constants::av::{
    AUDIO_BUFFER_SIZE, AUDIO_SAMPLE_HZ, DEFAULT_TIMEOUT, FULL_BUFF_SIZE, PID_3DS, TARGET_FPS,
    VEND_OUT_IDX, VEND_OUT_REQ, VEND_OUT_VALUE, VIDEO_BUFFER_SIZE, VID_3DS, WINDOW_HEIGHT,
    WINDOW_WIDTH,
};
use minifb::Scale;
use minifb::ScaleMode;
use minifb::Window;
use minifb::WindowOptions;
use rodio::{OutputStream, Source};
use rusb::{DeviceHandle, GlobalContext};
use std::ops::Sub;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime};

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

pub fn serve_audio(sink: &rodio::Sink, audio_channel: &Receiver<[u8; AUDIO_BUFFER_SIZE]>) {
    for audio in audio_channel {
        // Swap endianness
        let i16_sample: Vec<i16> = audio
            .chunks_exact(2)
            .map(|chunk| (chunk[1] as i16) << 8 | (chunk[0] as i16))
            .collect();

        let (remaining_sample, _truncated) = i16_sample.split_at(AUDIO_BUFFER_SIZE / 2);

        // Set speed appropriately - might not ultimately be necessary.
        let audio_src =
            rodio::buffer::SamplesBuffer::new(2, AUDIO_SAMPLE_HZ, remaining_sample).speed(1.0);

        // If our audio starts getting behind due to hardware lag, reset before adding to sink
        if sink.len() > MAX_PERMITTED_AUDIO_FRAME_SAMPLE_DELAY_NUM {
            sink.clear();
            sink.play();
        }

        sink.append(audio_src);
    }
}

pub fn serve_video(window: &mut Window, video_channel: &Receiver<[u8; VIDEO_BUFFER_SIZE]>) {
    for video in video_channel {
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
        video_tx: &Sender<[u8; VIDEO_BUFFER_SIZE]>,
        audio_tx: &Sender<[u8; AUDIO_BUFFER_SIZE]>,
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
                    break;
                }
            }
        }

        let (vid_slice, audio_slice) = buff.split_at(VIDEO_BUFFER_SIZE);
        let mut vid_arr = [0u8; VIDEO_BUFFER_SIZE];
        vid_arr.copy_from_slice(vid_slice);
        // Add error handling
        video_tx.send(vid_arr).unwrap();

        let mut audio_arr = [0u8; AUDIO_BUFFER_SIZE];
        audio_arr.copy_from_slice(audio_slice);
        // Add error handling
        audio_tx.send(audio_arr).unwrap();
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
    // See if we can replace with exact
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

    // Start window
    let opts = CustomWindowOptions::new(true, true, Scale::X2, ScaleMode::AspectRatioStretch);

    let mut window =
        minifb::Window::new("Krab3DS", WINDOW_WIDTH, WINDOW_HEIGHT, opts.inner()).unwrap();
    window.set_target_fps(TARGET_FPS);

    // Start FPS
    let mut counter = FpsCounter::new();

    let (video_tx, video_rx) = mpsc::channel();
    let (audio_tx, audio_rx) = mpsc::channel();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        println!("called write control");
        ds.write_control();
        println!("called populate buffers");
        ds.populate_buffers(&video_tx, &audio_tx);
    });

    while window.is_open() && !window.is_key_down(minifb::Key::Escape) {
        serve_audio(&sink, &audio_rx);
        serve_video(&mut window, &video_rx);
        counter.maybe_print_fps();
        counter.increment_frame();
    }
    // Release interface
    // ds.handle.release_interface(ds.endpoint.iface).unwrap();
    // if ds.config.using_kernel_driver {
    //     ds.handle.attach_kernel_driver(ds.endpoint.iface).unwrap();
    // };
}
