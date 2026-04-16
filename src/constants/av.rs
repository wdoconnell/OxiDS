use std::time::Duration;

// Break out module properly

// pub const BULK_ENDPOINT_ADDRESS: u8 = 130;
pub const VID_3DS: u16 = 0x16D0;
pub const PID_3DS: u16 = 0x06A3;

// Errors
pub const CANNOT_FIND_3DS: &str = "unable to locate 3ds device";
pub const CANNOT_CONFIGURE_3DS: &str = "could not configure 3ds device";

// Consider if will work below 10fps
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
pub const VEND_OUT_REQ: u8 = 0x40;
pub const VEND_OUT_VALUE: u16 = 0;
pub const VEND_OUT_IDX: u16 = 0;

pub const VIDEO_WIDTH: usize = 240;
pub const VIDEO_HEIGHT: usize = 720;
pub const RGB_COLOR_SIZE: usize = 3;
pub const VIDEO_BUFFER_SIZE: usize = VIDEO_WIDTH * VIDEO_HEIGHT * RGB_COLOR_SIZE;

pub const AUDIO_BUFFER_SIZE: usize = 4376;
pub const AUDIO_SAMPLE_HZ: u32 = 32728;
pub const AUDIO_NUM_ZEROES_END_DELIMETER: usize = 256;
pub const MAX_QUEUED_FRAMES: usize = 5;
pub const VIDEO_THREAD_STACK_SIZE: usize = 1024 * 1024 * 10;
pub const AUDIO_THREAD_STACK_SIZE: usize = 1024 * 1024 * 2;

pub const FULL_BUFF_SIZE: usize = VIDEO_BUFFER_SIZE + AUDIO_BUFFER_SIZE;

pub const WINDOW_HEIGHT: usize = 240;
pub const WINDOW_WIDTH: usize = 720;

// Maximize
pub const TARGET_FPS: usize = 80;
