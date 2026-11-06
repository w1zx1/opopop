//! opopop — 全画面動画キオスク。
//!
//! プログラムの動作:
//!   * 起動時に全画面（全モニター）で、すべてのウィンドウより前面に表示される;
//!   * exe に埋め込まれた動画 `opopop.mp4` を音声付きでループ再生する;
//!   * キーボードをフックして `Ctrl+Shift+Esc` 以外のすべてのキーを無効化する;
//!   * `Ctrl+Shift+Esc` でウィンドウが自ら閉じ、プログラムが終了する。
//!
//! 通常のアプリケーションから `Ctrl+Alt+Del` を横取りすることはできない
//! （Windows の保護されたシーケンスで、winlogon が処理する）。これが唯一
//! 残る抜け道であり、タスクマネージャーも同様である。
//!
//! ビルド:  cargo build --release
//! 実行:    target\release\opopop.exe

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::{w, Error, BOOL, HSTRING};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, EnumDisplayMonitors, FillRect,
    GetMonitorInfoW, HDC, HGDIOBJ, HMONITOR, MONITORINFO, PAINTSTRUCT,
};
use windows::Win32::Media::MediaFoundation::{
    IMFPMediaPlayer, MFPCreateMediaPlayer, MFShutdown, MFStartup, MFP_MEDIAPLAYER_STATE_STOPPED,
    MFP_OPTION_NONE, MFP_POSITIONTYPE_100NS, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::System::Variant::VT_I8;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LSHIFT, VK_RCONTROL, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------------
// 動画は実行ファイルに埋め込まれている — exe の隣に何も置く必要はない。
// ---------------------------------------------------------------------------
const VIDEO_BYTES: &[u8] = include_bytes!("../opopop.mp4");

const TIMER_ID: usize = 1;
const TICK_MS: u32 = 150;

/// キーボードフックによって設定され、メッセージループで読み取られる。
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

/// 修飾キーの状態は自前で管理する: 低レベルフック内では
/// `GetAsyncKeyState` が押されたばかりの Ctrl/Shift を捉えられないことがある。
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);

/// プレイヤーは別スロットに保持し、ウィンドウプロシージャからアクセスできるようにする。
static PLAYER: Mutex<Option<SendPlayer>> = Mutex::new(None);
struct SendPlayer(IMFPMediaPlayer);
unsafe impl Send for SendPlayer {}

// ---------------------------------------------------------------------------
// エントリーポイント
// ---------------------------------------------------------------------------
fn main() {
    if let Err(message) = run() {
        unsafe { show_error(&message) };
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    unsafe { run_inner() }
}

unsafe fn run_inner() -> Result<(), String> {
    // 物理ピクセルで扱う — そうしないとスケーリング設定のあるモニターでウィンドウがずれる。
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    MFStartup(MF_VERSION, 0).map_err(|e| format!("Media Foundation не запустилась: {e}"))?;

    let video_path = extract_video()?;
    let screen = virtual_screen_bounds();

    let hinstance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;

    let class_name = w!("OpopopKioskWindow");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: HINSTANCE(hinstance.0),
        hbrBackground: Default::default(),
        lpszClassName: class_name,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(format!("RegisterClassW: {}", Error::from_thread()));
    }

    // 枠なし、最前面、Alt+Tab に表示されないウィンドウ。
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class_name,
        w!("opopop"),
        WS_POPUP | WS_VISIBLE,
        screen.left,
        screen.top,
        screen.width,
        screen.height,
        None,
        None,
        Some(HINSTANCE(hinstance.0)),
        None,
    )
    .map_err(|e| format!("CreateWindowExW: {e}"))?;

    // --- プレイヤー --------------------------------------------------------
    let url = HSTRING::from(video_path.to_string_lossy().to_string());
    let mut raw_player: Option<IMFPMediaPlayer> = None;
    MFPCreateMediaPlayer(
        &url,
        true, // すぐに再生開始
        MFP_OPTION_NONE,
        None,
        Some(hwnd),
        Some(&mut raw_player),
    )
    .map_err(|e| format!("MFPCreateMediaPlayer: {e}"))?;

    let player = raw_player.ok_or_else(|| "Media Foundation не создала плеер".to_string())?;
    let _ = player.SetVolume(1.0);
    let _ = player.SetMute(false);
    let _ = player.SetBorderColor(COLORREF(0x0000_0000));
    let _ = player.Play();
    *PLAYER.lock().unwrap() = Some(SendPlayer(player));

    // --- キーボードのブロック ----------------------------------------------
    // WH_KEYBOARD_LL は下のメッセージループを持つスレッドに紐づく。
    SetWindowsHookExW(
        WH_KEYBOARD_LL,
        Some(low_level_keyboard_proc),
        Some(HINSTANCE(hinstance.0)),
        0,
    )
    .map_err(|e| format!("Не удалось перехватить клавиатуру: {e}"))?;

    // カーソルを隠す。
    ShowCursor(false);

    // 動画が再生されている間は画面をスリープさせない。
    let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);

    let _ = SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    force_foreground(hwnd);

    // --- メッセージループ --------------------------------------------------
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 <= 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // --- 正常終了 ----------------------------------------------------------
    let _ = SetThreadExecutionState(ES_CONTINUOUS);
    let _ = KillTimer(Some(hwnd), TIMER_ID);
    ShowCursor(true);
    if let Some(p) = PLAYER.lock().unwrap().take() {
        let _ = p.0.Shutdown();
    }
    let _ = MFShutdown();

    Ok(())
}

// ---------------------------------------------------------------------------
// ウィンドウプロシージャ
// ---------------------------------------------------------------------------
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_ID => {
            tick(hwnd);
            LRESULT(0)
        }
        // 外側からウィンドウを閉じることはできない — `Ctrl+Shift+Esc` のみ。
        WM_CLOSE => LRESULT(0),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let _ = SetCursor(None);
            LRESULT(1)
        }
        WM_ERASEBKGND => {
            paint_black(hwnd, HDC(wparam.0 as *mut c_void));
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_black(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// クライアント領域を黒で塗りつぶす（動画の周囲の背景）。
unsafe fn paint_black(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        let brush = CreateSolidBrush(COLORREF(0x0000_0000));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

/// 150 ms ごと: ウィンドウを最前面かつフォーカス状態に保ち、終了を確認し、
/// 動画が最後まで再生されたら再生をやり直す。
unsafe fn tick(hwnd: HWND) {
    if SHOULD_EXIT.load(Ordering::SeqCst) {
        PostQuitMessage(0);
        return;
    }

    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    force_foreground(hwnd);

    if let Ok(guard) = PLAYER.lock() {
        if let Some(p) = guard.as_ref() {
            if let Ok(state) = p.0.GetState() {
                if state == MFP_MEDIAPLAYER_STATE_STOPPED {
                    let mut position = PROPVARIANT::default();
                    position.Anonymous = PROPVARIANT_0 {
                        Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                            vt: VT_I8,
                            ..Default::default()
                        }),
                    };
                    let _ = p.0.SetPosition(&MFP_POSITIONTYPE_100NS, &position);
                    let _ = p.0.Play();
                }
            }
        }
    }
}

/// Windows が別のウィンドウにフォーカスを渡そうとしても、前面を奪い返す。
unsafe fn force_foreground(hwnd: HWND) {
    if GetForegroundWindow() == hwnd {
        return;
    }

    let foreground = GetForegroundWindow();
    let foreground_thread = if foreground.0.is_null() {
        0
    } else {
        GetWindowThreadProcessId(foreground, None)
    };
    let this_thread = GetCurrentThreadId();

    if foreground_thread != 0 && foreground_thread != this_thread {
        let _ = AttachThreadInput(foreground_thread, this_thread, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(foreground_thread, this_thread, false);
    } else {
        let _ = SetForegroundWindow(hwnd);
    }

    let _ = BringWindowToTop(hwnd);
    let _ = SetFocus(Some(hwnd));
}

// ---------------------------------------------------------------------------
// キーボード無効化
// ---------------------------------------------------------------------------
unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let is_key_down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = info.vkCode;

    if vk == VK_CONTROL.0 as u32 || vk == VK_LCONTROL.0 as u32 || vk == VK_RCONTROL.0 as u32 {
        CTRL_DOWN.store(is_key_down, Ordering::SeqCst);
    } else if vk == VK_SHIFT.0 as u32 || vk == VK_LSHIFT.0 as u32 || vk == VK_RSHIFT.0 as u32 {
        SHIFT_DOWN.store(is_key_down, Ordering::SeqCst);
    } else if vk == VK_ESCAPE.0 as u32
        && is_key_down
        && CTRL_DOWN.load(Ordering::SeqCst)
        && SHIFT_DOWN.load(Ordering::SeqCst)
    {
        // 唯一許可された組み合わせ: Ctrl+Shift+Esc -> 終了する。
        SHOULD_EXIT.store(true, Ordering::SeqCst);
    }

    // すべてを飲み込む: Win、Alt+Tab、Alt+F4、Ctrl+Esc、F12 など。
    LRESULT(1)
}

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct ScreenBounds {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

/// すべてのモニターを包含する矩形（物理ピクセル単位）。
unsafe fn virtual_screen_bounds() -> ScreenBounds {
    let mut acc = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(monitor_enum_proc),
        LPARAM(&mut acc as *mut _ as isize),
    );

    if acc.0 == i32::MAX {
        // フォールバック — プライマリモニター。
        return ScreenBounds {
            left: 0,
            top: 0,
            width: GetSystemMetrics(SM_CXSCREEN),
            height: GetSystemMetrics(SM_CYSCREEN),
        };
    }

    ScreenBounds {
        left: acc.0,
        top: acc.1,
        width: acc.2 - acc.0,
        height: acc.3 - acc.1,
    }
}

unsafe extern "system" fn monitor_enum_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let acc = &mut *(data.0 as *mut (i32, i32, i32, i32));
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        acc.0 = acc.0.min(info.rcMonitor.left);
        acc.1 = acc.1.min(info.rcMonitor.top);
        acc.2 = acc.2.max(info.rcMonitor.right);
        acc.3 = acc.3.max(info.rcMonitor.bottom);
    }
    BOOL(1)
}

/// 埋め込まれた動画を一時フォルダに展開する（Media Foundation はメモリから
/// ではなく URL/パスからしか再生できない）。
fn extract_video() -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join("opopop_embedded_video.mp4");
    let up_to_date = std::fs::metadata(&path)
        .map(|m| m.len() == VIDEO_BYTES.len() as u64)
        .unwrap_or(false);

    if !up_to_date {
        std::fs::write(&path, VIDEO_BYTES)
            .map_err(|e| format!("Не удалось распаковать видео в {}: {e}", path.display()))?;
    }

    Ok(path)
}

unsafe fn show_error(message: &str) {
    let text = HSTRING::from(message);
    let title = w!("opopop — ошибка");
    let _ = MessageBoxW(None, &text, title, MB_OK | MB_ICONERROR);
}
