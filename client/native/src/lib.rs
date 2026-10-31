pub mod decoder;
pub mod discovery;
pub mod handshake;
pub mod transport;

use jni::objects::{JClass, JFloatArray, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::JNIEnv;
use std::sync::OnceLock;
use tracing::info;
use xdisplay_common::input::{actions, tool_types, PointerEvent};

static CLIENT: OnceLock<transport::TransportClient> = OnceLock::new();
static DISCOVERY: OnceLock<discovery::Discovery> = OnceLock::new();

fn get_discovery() -> &'static discovery::Discovery {
    DISCOVERY.get_or_init(discovery::Discovery::default)
}

fn jstring_or_empty(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(String::from).unwrap_or_default()
}

/// Parses `host_id_hex:secret_hex` lines (newline or comma separated); malformed entries are skipped.
fn parse_credentials(text: &str) -> Vec<(xdisplay_common::HostId, xdisplay_common::DeviceSecret)> {
    text.split(['\n', ','])
        .filter_map(|entry| {
            let (id, secret) = entry.trim().split_once(':')?;
            Some((xdisplay_common::hex::decode_array(id)?, xdisplay_common::hex::decode_array(secret)?))
        })
        .collect()
}

fn get_client() -> &'static transport::TransportClient {
    CLIENT.get_or_init(transport::TransportClient::new)
}

/// Native JNI interface between the Android Kotlin Activity / SurfaceView
/// and the high-performance Rust streaming engine.
#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_initNativeLogging(
    _env: JNIEnv,
    _class: JClass,
) {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("XDisplayNative"),
        );
    }
    info!("XDisplay Android Native Engine initialized");
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_setSurface(
    #[allow(unused_variables)]
    env: JNIEnv,
    _class: JClass,
    #[allow(unused_variables)]
    surface: JObject,
) {
    #[cfg(target_os = "android")]
    {
        let window = if !surface.is_null() {
            unsafe {
                let raw_env = env.get_raw();
                let raw_obj = surface.as_raw();
                let win = ndk_sys::ANativeWindow_fromSurface(raw_env as *mut _, raw_obj as *mut _);
                if win.is_null() {
                    None
                } else {
                    Some(win)
                }
            }
        } else {
            None
        };
        get_client().set_surface(window);
    }
}


#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_startClient(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    device_name: JString,
    screen_width: jint,
    screen_height: jint,
    max_refresh_rate: jint,
    dpi: jint,
    supports_stylus: jboolean,
    client_id: JString,
    pin: JString,
    credentials: JString,
) {
    let host_str: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => "127.0.0.1".into(),
    };
    let dev_str: String = match env.get_string(&device_name) {
        Ok(s) => s.into(),
        Err(_) => "Android Tablet".into(),
    };

    let client_id_text = jstring_or_empty(&mut env, &client_id);
    let pin = jstring_or_empty(&mut env, &pin);
    let credentials = parse_credentials(&jstring_or_empty(&mut env, &credentials));
    // A malformed id would make every pairing unrecognizable later; fall back to a throwaway one.
    let client_id = xdisplay_common::hex::decode_array(&client_id_text)
        .or_else(|| xdisplay_common::random_bytes().ok())
        .unwrap_or([0; 16]);

    let port = (port as u32).clamp(1, 65535) as u16;
    let screen_width = (screen_width.max(100) as u32).min(16384);
    let screen_height = (screen_height.max(100) as u32).min(16384);
    let max_refresh_rate = (max_refresh_rate.max(30) as u32).min(360);
    let dpi = (dpi.max(50) as u32).min(1000);

    let config = transport::ClientConfig {
        host: host_str,
        port,
        device_name: dev_str,
        screen_width,
        screen_height,
        max_refresh_rate,
        dpi,
        supports_stylus: supports_stylus != 0,
        client_id,
        pin: if pin.trim().is_empty() { None } else { Some(pin) },
        credentials,
    };

    log::info!(
        "Starting native client: {:?} (Res: {}x{} @ {}Hz)",
        config.device_name,
        config.screen_width,
        config.screen_height,
        config.max_refresh_rate
    );
    get_client().start(config);
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_stopClient(
    _env: JNIEnv,
    _class: JClass,
) {
    get_client().stop();
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getConnectionStatus(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    get_client().get_status() as jint
}

/// Why the link is in `STATUS_NEEDS_PIN` / `STATUS_AUTH_FAILED` (empty otherwise).
#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getLastError<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    env.new_string(get_client().last_error()).unwrap_or_default()
}

/// Name of the PC the link is connected (or was last connected) to.
#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getHostName<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    env.new_string(get_client().host_name()).unwrap_or_default()
}

/// Returns `host_id_hex:secret_hex` once after a pairing completes, else an empty string.
/// The caller must persist it: it is what lets the tablet reconnect without a PIN.
#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_takeNewCredential<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    env.new_string(get_client().take_new_credential().unwrap_or_default()).unwrap_or_default()
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_startDiscovery(_env: JNIEnv, _class: JClass) {
    get_discovery().start();
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_stopDiscovery(_env: JNIEnv, _class: JClass) {
    get_discovery().stop();
}

/// Hosts currently announcing themselves, one `ip|port|name|host_id_hex` per line.
#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getDiscoveredHosts<'a>(
    env: JNIEnv<'a>,
    _class: JClass<'a>,
) -> JString<'a> {
    env.new_string(get_discovery().snapshot()).unwrap_or_default()
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getSentPacketsCount(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    get_client().get_sent_count() as jlong
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getRoundTripLatencyUs(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    get_client().get_rtt_us() as jlong
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_sendPointerEvent(
    _env: JNIEnv,
    _class: JClass,
    pointer_id: jint,
    action_raw: jint,
    tool_type_raw: jint,
    norm_x: jfloat,
    norm_y: jfloat,
    pressure: jfloat,
    tilt_x: jfloat,
    tilt_y: jfloat,
    buttons: jint,
    timestamp_us: jlong,
) {
    let action = match action_raw {
        0 => actions::DOWN,
        1 => actions::MOVE,
        2 => actions::UP,
        3 => actions::HOVER_ENTER,
        4 => actions::HOVER_MOVE,
        5 => actions::HOVER_EXIT,
        _ => actions::CANCEL,
    };

    let tool_type = match tool_type_raw {
        1 => tool_types::FINGER,
        2 => tool_types::STYLUS,
        // Android MotionEvent.TOOL_TYPE_*: MOUSE = 3, ERASER = 4
        3 => tool_types::MOUSE,
        4 => tool_types::ERASER,
        _ => tool_types::UNKNOWN,
    };

    let norm_x = if norm_x.is_finite() { norm_x.clamp(0.0, 1.0) } else { 0.0 };
    let norm_y = if norm_y.is_finite() { norm_y.clamp(0.0, 1.0) } else { 0.0 };
    let pressure = if pressure.is_finite() { pressure.clamp(0.0, 1.0) } else { 0.0 };
    let tilt_x = if tilt_x.is_finite() { tilt_x.clamp(-90.0, 90.0) } else { 0.0 };
    let tilt_y = if tilt_y.is_finite() { tilt_y.clamp(-90.0, 90.0) } else { 0.0 };

    let event = PointerEvent {
        timestamp_us: timestamp_us as u64,
        pointer_id: pointer_id as u32,
        action,
        tool_type,
        norm_x,
        norm_y,
        pressure,
        tilt_x,
        tilt_y,
        buttons: buttons as u16,
        reserved: 0,
    };

    get_client().queue_pointer_event(event);
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_sendKeyEvent(
    _env: JNIEnv,
    _class: JClass,
    vk_code: jint,
    action: jint,
    modifiers: jint,
) {
    let event = xdisplay_common::input::KeyEvent::new(
        vk_code as u16,
        action as u8,
        modifiers as u8,
    );
    get_client().queue_key_event(event);
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_updateStreamSettings(
    _env: JNIEnv,
    _class: JClass,
    target_fps: jint,
    bitrate_kbps: jint,
) {
    // Kotlin passes -1 (NativeEngine.BITRATE_AUTO) for automatic bitrate and 0 for "unchanged".
    let bitrate = match bitrate_kbps {
        b if b < 0 => Some(xdisplay_common::protocol::BITRATE_AUTO),
        0 => None,
        b => Some(b as u32),
    };
    get_client().update_stream_settings(target_fps.max(0) as u32, bitrate);
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_sendScrollEvent(
    _env: JNIEnv,
    _class: JClass,
    delta_y: jfloat,
) {
    get_client().queue_scroll_event(delta_y);
}

#[no_mangle]
pub extern "system" fn Java_com_xdisplay_NativeEngine_getCursorState(
    env: JNIEnv,
    _class: JClass,
    out_state: JFloatArray,
) -> jboolean {
    let cursor = get_client().get_cursor_state();
    if cursor.visible == 0 {
        return 0;
    }
    let data: [f32; 4] = [
        cursor.norm_x,
        cursor.norm_y,
        cursor.shape_type as f32,
        cursor.visible as f32,
    ];
    let _ = env.set_float_array_region(&out_state, 0, &data);
    1
}
