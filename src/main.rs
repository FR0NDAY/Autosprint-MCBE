use std::ffi::CStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use directories::BaseDirs;
use windows::core::PCSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    CreateEventA, GetCurrentProcess, GetCurrentThread, ResetEvent, SetEvent, SetPriorityClass,
    SetThreadPriority, Sleep, SwitchToThread, WaitForSingleObject, ABOVE_NORMAL_PRIORITY_CLASS,
    INFINITE, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextA, MapVirtualKeyA, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageA, GetClassNameA, GetCursorInfo, GetForegroundWindow,
    GetMessageA, GetWindowTextA, SetWindowsHookExA, TranslateMessage, UnhookWindowsHookEx,
    CURSORINFO, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

static FORWARD_KEY: AtomicI32 = AtomicI32::new(87);
static SPRINT_KEY: AtomicI32 = AtomicI32::new(17);
static FORWARD_PRESSED: AtomicBool = AtomicBool::new(false);
static SPRINT_HELD: AtomicBool = AtomicBool::new(false);
static FORWARD_EVENT: OnceLock<HANDLE> = OnceLock::new();
static LATENCY_MODE: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy)]
enum LatencyMode {
    Balanced,
    Ultra,
}

impl LatencyMode {
    fn from_str(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("balanced") {
            Some(Self::Balanced)
        } else if value.eq_ignore_ascii_case("ultra") {
            Some(Self::Ultra)
        } else {
            None
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Ultra => "ultra",
        }
    }

    fn burst_ms(self) -> u64 {
        match self {
            Self::Balanced => 25,
            Self::Ultra => 90,
        }
    }

    fn spin_iters(self) -> u32 {
        match self {
            Self::Balanced => 64,
            Self::Ultra => 256,
        }
    }

    fn post_burst_sleep_ms(self) -> u32 {
        match self {
            Self::Balanced => 1,
            Self::Ultra => 0,
        }
    }
}

fn get_latency_mode() -> LatencyMode {
    if LATENCY_MODE.load(Ordering::Relaxed) == 1 {
        LatencyMode::Ultra
    } else {
        LatencyMode::Balanced
    }
}

fn set_latency_mode(mode: LatencyMode) {
    let raw = match mode {
        LatencyMode::Balanced => 0,
        LatencyMode::Ultra => 1,
    };
    LATENCY_MODE.store(raw, Ordering::Relaxed);
}

fn print_usage() {
    println!("Usage: autosprint-mcbe.exe [--latency balanced|ultra]");
    println!("  --latency balanced    Default mode, lower CPU usage");
    println!("  --latency ultra       Most aggressive low-latency mode");
}

fn parse_latency_mode_from_args() -> Option<LatencyMode> {
    let mut mode = LatencyMode::Balanced;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            print_usage();
            return None;
        }

        if arg == "--latency" {
            let Some(value) = args.next() else {
                eprintln!("Error: missing value for --latency. Use balanced or ultra.");
                return None;
            };
            let Some(parsed) = LatencyMode::from_str(&value) else {
                eprintln!("Error: invalid --latency value '{value}'. Use balanced or ultra.");
                return None;
            };
            mode = parsed;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--latency=") {
            let Some(parsed) = LatencyMode::from_str(value) else {
                eprintln!("Error: invalid --latency value '{value}'. Use balanced or ultra.");
                return None;
            };
            mode = parsed;
            continue;
        }

        eprintln!("Warning: unknown argument '{arg}' ignored.");
    }
    Some(mode)
}

fn build_path(a: &Path, b: &str) -> PathBuf {
    a.join(b)
}

fn contains_case_insensitive(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn try_options_candidate(path: &Path, newest: &mut SystemTime, best: &mut Option<PathBuf>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if best.is_none() || modified > *newest {
        *newest = modified;
        *best = Some(path.to_path_buf());
    }
}

fn scan_options_recursive(
    root: &Path,
    depth: u8,
    newest: &mut SystemTime,
    best: &mut Option<PathBuf>,
) {
    if depth > 10 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    let root_text = root.to_string_lossy().to_ascii_lowercase();
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        let full_path = entry.path();

        if file_type.is_dir() {
            if file_type.is_symlink() {
                continue;
            }
            let child_text = name_text.to_ascii_lowercase();
            let minecraft_context = root_text.contains("minecraft")
                || root_text.contains("mojang")
                || child_text.contains("minecraft")
                || child_text.contains("mojang");
            if depth < 2 || minecraft_context {
                scan_options_recursive(&full_path, depth + 1, newest, best);
            }
            continue;
        }

        if file_type.is_file()
            && name_text.eq_ignore_ascii_case("options.txt")
            && contains_case_insensitive(&full_path.to_string_lossy(), "minecraftpe")
        {
            try_options_candidate(&full_path, newest, best);
        }
    }
}

fn scan_known_bedrock_users(
    roaming_path: &Path,
    newest: &mut SystemTime,
    best: &mut Option<PathBuf>,
) {
    let users_root = build_path(&build_path(roaming_path, "Minecraft Bedrock"), "Users");
    let Ok(entries) = fs::read_dir(users_root) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let candidate = entry
            .path()
            .join("games")
            .join("com.mojang")
            .join("minecraftpe")
            .join("options.txt");
        try_options_candidate(&candidate, newest, best);
    }
}

fn find_options_path() -> Option<PathBuf> {
    let base_dirs = BaseDirs::new()?;
    let local_path = base_dirs.data_local_dir().to_path_buf();
    let roaming_path = base_dirs.data_dir().to_path_buf();

    let mut newest = SystemTime::UNIX_EPOCH;
    let mut best: Option<PathBuf> = None;

    let local_candidates = [
        local_path
            .join("Packages")
            .join("Microsoft.MinecraftUWP_8wekyb3d8bbwe")
            .join("LocalState")
            .join("games")
            .join("com.mojang")
            .join("minecraftpe")
            .join("options.txt"),
        local_path
            .join("Packages")
            .join("Microsoft.MinecraftWindowsBeta_8wekyb3d8bbwe")
            .join("LocalState")
            .join("games")
            .join("com.mojang")
            .join("minecraftpe")
            .join("options.txt"),
    ];

    for candidate in local_candidates {
        try_options_candidate(&candidate, &mut newest, &mut best);
    }

    scan_known_bedrock_users(&roaming_path, &mut newest, &mut best);
    scan_options_recursive(&roaming_path, 0, &mut newest, &mut best);
    scan_options_recursive(&local_path, 0, &mut newest, &mut best);

    best
}

fn get_key_name(vk_code: i32) -> String {
    if vk_code == 0x11 || vk_code == 0xA2 || vk_code == 0xA3 {
        return "Control".to_string();
    }
    if vk_code == 0x10 || vk_code == 0xA0 || vk_code == 0xA1 {
        return "Shift".to_string();
    }
    if vk_code == 0x12 || vk_code == 0xA4 || vk_code == 0xA5 {
        return "Alt".to_string();
    }

    unsafe {
        let scan_code = MapVirtualKeyA(vk_code as u32, MAPVK_VK_TO_VSC);
        let mut buffer = [0u8; 128];
        let len = GetKeyNameTextA((scan_code << 16) as i32, &mut buffer);
        if len > 0 {
            CStr::from_bytes_until_nul(&buffer)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| format!("Unknown ({})", vk_code))
        } else {
            format!("Unknown ({})", vk_code)
        }
    }
}

fn is_cursor_hidden() -> bool {
    unsafe {
        let mut cursor_info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut cursor_info).is_ok() {
            return cursor_info.flags.0 == 0;
        }
        false
    }
}

fn is_minecraft_focused() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0 == 0 {
            return false;
        }

        let mut class_name = [0u8; 256];
        if GetClassNameA(hwnd, &mut class_name) == 0 {
            return false;
        }

        let class_name = match CStr::from_bytes_until_nul(&class_name)
            .ok()
            .and_then(|s| s.to_str().ok())
        {
            Some(v) => v,
            None => return false,
        };

        let class_ok = class_name == "Bedrock"
            || class_name == "ApplicationFrameWindow"
            || class_name == "Windows.UI.Core.CoreWindow";
        if !class_ok {
            return false;
        }

        let mut title = [0u8; 256];
        if GetWindowTextA(hwnd, &mut title) == 0 {
            return false;
        }

        match CStr::from_bytes_until_nul(&title)
            .ok()
            .and_then(|s| s.to_str().ok())
        {
            Some(v) => v == "Minecraft",
            None => false,
        }
    }
}

fn send_key(vk_code: i32, down: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk_code as u16),
                wScan: 0,
                dwFlags: if down {
                    Default::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn should_sprint_now() -> bool {
    is_minecraft_focused() && is_cursor_hidden()
}

fn update_sprint_state(should_sprint: bool) {
    let mut current = SPRINT_HELD.load(Ordering::Acquire);
    while current != should_sprint {
        match SPRINT_HELD.compare_exchange(
            current,
            should_sprint,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                let sprint_key = SPRINT_KEY.load(Ordering::Relaxed);
                send_key(sprint_key, should_sprint);
                return;
            }
            Err(observed) => current = observed,
        }
    }
}

fn tight_wait(spin_iters: u32) {
    for _ in 0..spin_iters {
        std::hint::spin_loop();
    }
    unsafe {
        if !SwitchToThread().as_bool() {
            Sleep(0);
        }
    }
}

fn relaxed_wait(sleep_ms: u32) {
    unsafe {
        if sleep_ms == 0 {
            if !SwitchToThread().as_bool() {
                Sleep(0);
            }
        } else {
            Sleep(sleep_ms);
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let kb_data = *(l_param.0 as *const KBDLLHOOKSTRUCT);
        if kb_data.vkCode as i32 == FORWARD_KEY.load(Ordering::Relaxed) {
            let msg = w_param.0 as u32;
            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                if !FORWARD_PRESSED.swap(true, Ordering::SeqCst) {
                    update_sprint_state(should_sprint_now());
                    if let Some(event) = FORWARD_EVENT.get() {
                        let _ = SetEvent(*event);
                    }
                }
            } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                FORWARD_PRESSED.store(false, Ordering::SeqCst);
                update_sprint_state(false);
            }
        }
    }

    CallNextHookEx(HHOOK::default(), n_code, w_param, l_param)
}

fn sprint_loop() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }

    let latency_mode = get_latency_mode();

    loop {
        let Some(event) = FORWARD_EVENT.get() else {
            return;
        };

        unsafe {
            let _ = WaitForSingleObject(*event, INFINITE);
        }

        let burst_deadline = Instant::now() + Duration::from_millis(latency_mode.burst_ms());
        while FORWARD_PRESSED.load(Ordering::Acquire) {
            update_sprint_state(should_sprint_now());
            if Instant::now() < burst_deadline {
                tight_wait(latency_mode.spin_iters());
            } else {
                relaxed_wait(latency_mode.post_burst_sleep_ms());
            }
        }

        update_sprint_state(false);

        unsafe {
            let _ = ResetEvent(*event);
        }
    }
}

fn parse_options(path: &Path) -> (Option<i32>, Option<i32>) {
    let Ok(file) = fs::File::open(path) else {
        return (None, None);
    };
    let reader = BufReader::new(file);

    let mut forward_key = None;
    let mut sprint_key = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.contains("keyboard_type_0_key.forward") {
            if let Some(val_str) = line.split(':').nth(1) {
                if let Ok(val) = val_str.trim().parse::<i32>() {
                    if val != 0 {
                        forward_key = Some(val);
                    }
                }
            }
        } else if line.contains("keyboard_type_0_key.sprint") {
            if let Some(val_str) = line.split(':').nth(1) {
                if let Ok(val) = val_str.trim().parse::<i32>() {
                    if val != 0 {
                        sprint_key = Some(val);
                    }
                }
            }
        }
    }

    (forward_key, sprint_key)
}

fn prompt_key(prompt: &str, default_value: i32) -> i32 {
    print!("{prompt}");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        if let Ok(parsed) = input.trim().parse::<i32>() {
            if parsed != 0 {
                return parsed;
            }
        }
    }
    default_value
}

fn main() {
    let Some(latency_mode) = parse_latency_mode_from_args() else {
        return;
    };
    set_latency_mode(latency_mode);

    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }

    let event_handle = match unsafe { CreateEventA(None, true, false, PCSTR::null()) } {
        Ok(handle) => handle,
        Err(_) => {
            eprintln!("Error: failed to create synchronization event.");
            return;
        }
    };

    let _ = FORWARD_EVENT.set(event_handle);

    let mut forward_key = 87;
    let mut sprint_key = 17;
    let mut found_forward = false;
    let mut found_sprint = false;

    if let Some(path) = find_options_path() {
        println!("[Config] options.txt: {}", path.display());
        let (file_forward, file_sprint) = parse_options(&path);
        if let Some(v) = file_forward {
            forward_key = v;
            found_forward = true;
        }
        if let Some(v) = file_sprint {
            sprint_key = v;
            found_sprint = true;
        }
    }

    if !found_forward || !found_sprint {
        forward_key = prompt_key("Manual keys: Forward (87=W): ", forward_key);
        sprint_key = prompt_key("Sprint (17=Ctrl): ", sprint_key);
    }

    if forward_key == 0 {
        forward_key = 87;
    }
    if sprint_key == 0 {
        sprint_key = 17;
    }

    FORWARD_KEY.store(forward_key, Ordering::Relaxed);
    SPRINT_KEY.store(sprint_key, Ordering::Relaxed);

    println!("---------------------------------------------------");
    println!("[Config] Detected Forward: {}", get_key_name(forward_key));
    println!("[Config] Detected Sprint:  {}", get_key_name(sprint_key));
    println!("[Config] Latency Mode:   {}", latency_mode.as_str());
    println!("---------------------------------------------------");
    println!("Status: Running...");

    thread::spawn(sprint_loop);

    unsafe {
        let hook = match SetWindowsHookExA(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            HINSTANCE(0),
            0,
        ) {
            Ok(hook) => hook,
            Err(_) => {
                eprintln!("Error: failed to install keyboard hook.");
                let _ = CloseHandle(event_handle);
                return;
            }
        };

        let mut msg = MSG::default();
        while GetMessageA(&mut msg, HWND(0), 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
        let _ = CloseHandle(event_handle);
    }
}
