//! Clew made to draw in software a window it made for a DirectComposition
//! visual, which shows none of it, starts again presenting directly
//! (`app::corners`, `shell::update`): the real program, on Windows.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible,
    WS_EX_NOREDIRECTIONBITMAP,
};

/// Clew is started choosing a visual whatever iced draws with, and iced
/// told to draw in software (`ICED_BACKEND`): as a GPU failing after Clew
/// asked for it would have it. That Clew quits by itself, and the Clew it
/// starts as it exits — its child, the same program — presents its
/// window directly: a window with a surface for GDI to draw to, which
/// shows what iced draws in software, and no relaunch of its own. Its
/// home and application data are the test's, not the user's.
#[test]
fn clew_drawn_in_software_through_a_visual_starts_again_presenting_directly() {
    let home = std::env::temp_dir().join(format!("clew-relaunch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("AppData").join("Roaming")).unwrap();
    std::fs::create_dir_all(home.join("AppData").join("Local")).unwrap();
    let log = home.join("clew.log");
    let mut first = clew(&home, &log);
    let first_pid = first.id();
    let mut started = Started(vec![first_pid]);

    let quit = wait(Duration::from_secs(90), || first.try_wait().ok().flatten());
    let Some(status) = quit else {
        panic!(
            "the Clew drawn in software through a visual did not quit by itself: {}",
            said(&log)
        );
    };
    // Gone: its id may be another process's by now.
    started.0.retain(|&pid| pid != first_pid);
    assert!(status.success(), "it quit {status}: {}", said(&log));

    let relaunched = wait(Duration::from_secs(10), || {
        clews().into_iter().find(|&(_, parent)| parent == first_pid)
    });
    let Some((second_pid, _)) = relaunched else {
        panic!("it started no Clew as it quit: {}", said(&log));
    };
    started.0.push(second_pid);

    let window = wait(Duration::from_secs(90), || {
        windows_of(second_pid).into_iter().next()
    });
    let Some(window) = window else {
        panic!("the Clew it started shows no window: {}", said(&log));
    };
    // SAFETY: a window of a live process; only its style is read.
    let ex_style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } as u32;
    assert_eq!(
        ex_style & WS_EX_NOREDIRECTIONBITMAP,
        0,
        "the Clew it started made its window for a visual again: {}",
        said(&log)
    );
    // It stays, and starts nothing of its own.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        clews().iter().any(|&(pid, _)| pid == second_pid),
        "the Clew it started did not stay: {}",
        said(&log)
    );
    assert!(
        !clews().iter().any(|&(_, parent)| parent == second_pid),
        "the Clew it started started another"
    );
    drop(started);
    let _ = std::fs::remove_dir_all(&home);
}

/// Clew, started with `home` for its home and application data, told to
/// choose a visual whatever iced draws with and iced told to draw in
/// software, what it says written to `log`.
fn clew(home: &Path, log: &Path) -> Child {
    let out = std::fs::File::create(log).unwrap();
    Command::new(env!("CARGO_BIN_EXE_clew"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("CLEW_TEST_VISUAL_FOR_ANY_RENDERER", "1")
        .env("ICED_BACKEND", "tiny-skia")
        .env_remove("CLEW_PRESENT_DIRECTLY")
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("Clew starts")
}

/// The Clews started and still running, ended when the test is (by a
/// failure or not).
struct Started(Vec<u32>);

impl Drop for Started {
    fn drop(&mut self) {
        for &pid in &self.0 {
            // SAFETY: a process handle opened to end it, closed after.
            unsafe {
                let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !process.is_null() {
                    TerminateProcess(process, 1);
                    CloseHandle(process);
                }
            }
        }
    }
}

/// What `look` finds within `limit`, looked for every tenth of a second.
fn wait<T>(limit: Duration, mut look: impl FnMut() -> Option<T>) -> Option<T> {
    let until = Instant::now() + limit;
    loop {
        if let Some(found) = look() {
            return Some(found);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The running processes of Clew's program: each one's id and its
/// parent's.
fn clews() -> Vec<(u32, u32)> {
    let name = Path::new(env!("CARGO_BIN_EXE_clew"))
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let mut found = Vec::new();
    // SAFETY: a snapshot of the processes, walked and then closed; each
    // entry carries its size, as the walk requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
            let exe = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            if exe == name {
                found.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

/// The visible top-level windows of process `pid`.
fn windows_of(pid: u32) -> Vec<HWND> {
    unsafe extern "system" fn each(window: HWND, found: LPARAM) -> i32 {
        // SAFETY: `found` is the list `windows_of` lends for the walk.
        let found = unsafe { &mut *(found as *mut Vec<HWND>) };
        found.push(window);
        1
    }
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the callback is given the list above, alive for the walk.
    unsafe { EnumWindows(Some(each), &raw mut all as LPARAM) };
    all.into_iter()
        .filter(|&window| {
            let mut owner = 0u32;
            // SAFETY: a window the walk gave; its owner read into `owner`.
            unsafe { GetWindowThreadProcessId(window, &mut owner) };
            // SAFETY: as above.
            owner == pid && unsafe { IsWindowVisible(window) } != 0
        })
        .collect()
}

/// What Clew wrote to `log`, for a failure to show.
fn said(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(40).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    format!(
        "{} said:\n{}",
        PathBuf::from(log).display(),
        tail.join("\n")
    )
}
