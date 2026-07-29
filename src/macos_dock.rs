//! macOS Dock 右键窗口列表。
//!
//! winit 持有 NSApplicationDelegate，不能替换它。这里只在它的运行时类上补充
//! `applicationDockMenu:`，其它生命周期和事件方法仍由 winit 处理。

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSString};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// 应用失去激活状态前的前台窗口。Dock 再次激活应用时恢复它，避免 AppKit
/// 总是把最后创建的编辑器窗口提到主窗口前面。
static LAST_KEY_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Dock 回调发生在 AppKit 完成窗口排序之前，必须延迟到下一次主界面绘制末尾恢复。
static RESTORE_KEY_WINDOW_PENDING: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_window_menu() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(class) = AnyClass::get(c"WinitApplicationDelegate") else {
            return;
        };
        install_dock_menu_method(class);
        install_remember_key_window_method(class);
        install_restore_key_window_method(class);
    });
}

fn install_dock_menu_method(class: &AnyClass) {
    let selector = sel!(applicationDockMenu:);
    if class.instance_method(selector).is_some() {
        return;
    }
    let implementation: Imp = unsafe {
        std::mem::transmute::<
            unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSApplication) -> *mut NSMenu,
            Imp,
        >(dock_window_menu)
    };
    // SAFETY: 运行时类在进程生命周期内有效；类型编码对应
    // `NSMenu * method(id, SEL, NSApplication *)`。
    unsafe {
        let _ = objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            selector,
            implementation,
            c"@@:@".as_ptr(),
        );
    }
}

fn install_remember_key_window_method(class: &AnyClass) {
    let selector = sel!(applicationWillResignActive:);
    if class.instance_method(selector).is_some() {
        return;
    }
    let implementation: Imp = unsafe {
        std::mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject), Imp>(
            remember_key_window,
        )
    };
    // SAFETY: 类型编码对应 `void method(id, SEL, NSNotification *)`。
    unsafe {
        let _ = objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            selector,
            implementation,
            c"v@:@".as_ptr(),
        );
    }
}

fn install_restore_key_window_method(class: &AnyClass) {
    let selector = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    if class.instance_method(selector).is_some() {
        return;
    }
    let implementation: Imp = unsafe {
        std::mem::transmute::<
            unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSApplication, bool) -> bool,
            Imp,
        >(restore_key_window)
    };
    // SAFETY: macOS 的 BOOL 是 signed char；类型编码对应
    // `BOOL method(id, SEL, NSApplication *, BOOL)`。
    unsafe {
        let _ = objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            selector,
            implementation,
            c"c@:@c".as_ptr(),
        );
    }
}

unsafe extern "C-unwind" fn dock_window_menu(
    _delegate: &AnyObject,
    _cmd: Sel,
    app: &NSApplication,
) -> *mut NSMenu {
    let mtm = MainThreadMarker::new().expect("Dock menu callback must run on the main thread");
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("窗口"));
    let empty = NSString::from_str("");

    for window in app.windows().iter() {
        let title = window.title();
        if title.length() == 0 || !window.isVisible() {
            continue;
        }
        window.setExcludedFromWindowsMenu(false);
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &title,
                Some(sel!(makeKeyAndOrderFront:)),
                &empty,
            )
        };
        unsafe {
            item.setTarget(Some(window.as_ref()));
        }
        menu.addItem(&item);
    }

    Retained::autorelease_return(menu)
}

unsafe extern "C-unwind" fn remember_key_window(
    _delegate: &AnyObject,
    _cmd: Sel,
    _notification: &AnyObject,
) {
    let mtm = MainThreadMarker::new().expect("application callback must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    if let Some(window) = app.keyWindow() {
        LAST_KEY_WINDOW.store(window.windowNumber(), Ordering::Relaxed);
    }
}

unsafe extern "C-unwind" fn restore_key_window(
    _delegate: &AnyObject,
    _cmd: Sel,
    _app: &NSApplication,
    _has_visible_windows: bool,
) -> bool {
    // 此时 AppKit 尚未完成激活排序；这里只记录请求，主界面帧末尾再恢复。
    RESTORE_KEY_WINDOW_PENDING.store(true, Ordering::Release);
    // 不让 AppKit 执行默认的“重新打开”窗口选择。
    false
}

/// 在主 viewport 完成本帧（包括独立编辑器 viewport）之后调用，确保恢复动作
/// 晚于 AppKit 和 eframe 本轮所有窗口激活操作。
pub(crate) fn restore_dock_window_order_if_pending() {
    if !RESTORE_KEY_WINDOW_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let mtm = MainThreadMarker::new().expect("window restore must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let wanted = LAST_KEY_WINDOW.load(Ordering::Relaxed);
    if wanted != 0
        && let Some(window) = app
            .windows()
            .iter()
            .find(|window| window.windowNumber() == wanted)
    {
        if window.isMiniaturized() {
            window.deminiaturize(None);
        }
        window.makeKeyAndOrderFront(None);
    }
}
