use super::{UpdateAction, UpdateBackend, secure_update_url};
use crate::config::UpdateChannel;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{MainThreadMarker, NSBundle, NSObject, NSString};
use std::cell::Cell;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RShellSparkleUpdaterDelegate"]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<UpdateChannel>]
    struct SparkleDelegate;

    impl SparkleDelegate {
        /// Sparkle 推荐通过 delegate 提供可变 feed，而不是调用已弃用且可能产生
        /// 竞态的 `setFeedURL:`。
        #[unsafe(method(feedURLStringForUpdater:))]
        fn feed_url_string(&self, _updater: &AnyObject) -> *mut NSString {
            let Some(value) = MacOsBackend::feed_url(self.ivars().get()) else {
                return std::ptr::null_mut();
            };
            Retained::autorelease_return(NSString::from_str(value))
        }
    }
);

impl SparkleDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(Cell::new(UpdateChannel::Stable));
        // SAFETY: 初始化 NSObject 子类，ivars 已由 set_ivars 完整初始化。
        unsafe { msg_send![super(this), init] }
    }
}

pub struct MacOsBackend {
    controller: Option<Retained<AnyObject>>,
    // Sparkle 的 updaterDelegate 不由控制器强持有。
    delegate: Option<Retained<SparkleDelegate>>,
    // 保持 framework bundle 存活，不允许在控制器仍存在时卸载代码。
    _framework: Option<Retained<NSBundle>>,
    error: Option<String>,
    started: bool,
}

type LoadedSparkle = (
    Retained<AnyObject>,
    Retained<SparkleDelegate>,
    Retained<NSBundle>,
);

impl MacOsBackend {
    pub fn new() -> Self {
        match Self::load() {
            Ok((controller, delegate, framework)) => Self {
                controller: Some(controller),
                delegate: Some(delegate),
                _framework: Some(framework),
                error: None,
                started: false,
            },
            Err(error) => Self {
                controller: None,
                delegate: None,
                _framework: None,
                error: Some(error),
                started: false,
            },
        }
    }

    fn load() -> Result<LoadedSparkle, String> {
        let mtm = MainThreadMarker::new().ok_or_else(|| "Sparkle 必须在主线程初始化".to_owned())?;
        let frameworks = NSBundle::mainBundle()
            .privateFrameworksPath()
            .ok_or_else(|| "当前程序不是完整的 macOS App 包".to_owned())?;
        let path = format!("{frameworks}/Sparkle.framework");
        let framework = NSBundle::bundleWithPath(&NSString::from_str(&path))
            .ok_or_else(|| "未找到内嵌的 Sparkle.framework（开发构建不会启用更新）".to_owned())?;
        // SAFETY: 路径固定在当前签名 App 的 Frameworks 目录；发布流水线在启动前
        // 使用 codesign 验证整个 bundle，NSBundle 只加载该内嵌 framework。
        if !framework.isLoaded() && !unsafe { framework.load() } {
            return Err("Sparkle.framework 加载失败".to_owned());
        }

        let class = AnyClass::get(c"SPUStandardUpdaterController")
            .ok_or_else(|| "Sparkle 更新控制器不可用".to_owned())?;
        let delegate = SparkleDelegate::new(mtm);
        // SAFETY: 选择器来自 Sparkle 2 的稳定公开 API。alloc/init 返回 +1 对象，
        // 随后立即转为 Retained；更新 delegate 由本后端强持有，标准控制器提供 UI。
        let allocated: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let initialized: *mut AnyObject = unsafe {
            msg_send![allocated,
                initWithStartingUpdater: false,
                updaterDelegate: &*delegate,
                userDriverDelegate: std::ptr::null::<AnyObject>()
            ]
        };
        let controller = unsafe { Retained::from_raw(initialized) }
            .ok_or_else(|| "Sparkle 更新控制器初始化失败".to_owned())?;
        Ok((controller, delegate, framework))
    }

    fn feed_url(channel: UpdateChannel) -> Option<&'static str> {
        match channel {
            UpdateChannel::Stable => option_env!("RSHELL_MACOS_APPCAST_URL"),
            UpdateChannel::Beta => option_env!("RSHELL_MACOS_BETA_APPCAST_URL"),
        }
    }

    fn updater(&self) -> Result<*mut AnyObject, String> {
        let controller = self.controller.as_deref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| "Sparkle 不可用".to_owned())
        })?;
        // SAFETY: controller 是已初始化并由 Retained 持有的
        // SPUStandardUpdaterController，`updater` 是 Sparkle 2 的公开 getter。
        let updater: *mut AnyObject = unsafe { msg_send![controller, updater] };
        if updater.is_null() {
            Err("Sparkle updater 尚未就绪".to_owned())
        } else {
            Ok(updater)
        }
    }

    fn configure_feed(&self, channel: UpdateChannel) -> Result<(), String> {
        if let Some(value) = Self::feed_url(channel) {
            secure_update_url(value)?;
        } else if channel == UpdateChannel::Beta {
            return Err("此构建未配置测试版更新源".to_owned());
        }
        let delegate = self
            .delegate
            .as_deref()
            .ok_or_else(|| "Sparkle 更新 delegate 不可用".to_owned())?;
        delegate.ivars().set(channel);
        Ok(())
    }
}

impl UpdateBackend for MacOsBackend {
    fn platform_label(&self) -> &'static str {
        "macOS · Sparkle 2"
    }

    fn availability(&self, channel: UpdateChannel) -> Result<(), String> {
        self.updater()?;
        if channel == UpdateChannel::Beta && Self::feed_url(channel).is_none() {
            return Err("此构建未配置测试版更新源".to_owned());
        }
        if let Some(value) = Self::feed_url(channel) {
            secure_update_url(value)?;
        }
        Ok(())
    }

    fn check_for_updates(&mut self, channel: UpdateChannel) -> Result<UpdateAction, String> {
        if !self.started {
            return Err("Sparkle updater 尚未启动".to_owned());
        }
        self.configure_feed(channel)?;
        let controller = self
            .controller
            .as_deref()
            .ok_or_else(|| "Sparkle 不可用".to_owned())?;
        // SAFETY: controller 在主线程创建并仅由 eframe UI 线程调用；该 selector
        // 显示 Sparkle 标准更新 UI，不在 RShell 内处理安装文件。
        unsafe {
            let _: () = msg_send![controller, checkForUpdates: std::ptr::null::<AnyObject>()];
        }
        Ok(UpdateAction::Started)
    }

    fn configure_channel(&mut self, channel: UpdateChannel) -> Result<(), String> {
        self.availability(channel)?;
        self.configure_feed(channel)
    }

    fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        let controller = self
            .controller
            .as_deref()
            .ok_or_else(|| "Sparkle 不可用".to_owned())?;
        // SAFETY: controller 在主线程创建，feed 与自动检查选项均已在启动前设置。
        unsafe {
            let _: () = msg_send![controller, startUpdater];
        }
        self.started = true;
        Ok(())
    }

    fn manages_automatic_checks(&self) -> bool {
        self.controller.is_some()
    }

    fn set_automatic_checks(&mut self, enabled: bool) -> Result<(), String> {
        let updater = self.updater()?;
        // SAFETY: updater 在控制器生命周期内有效；setter 是 Sparkle 2 公开 API。
        unsafe {
            let _: () = msg_send![updater, setAutomaticallyChecksForUpdates: enabled];
        }
        Ok(())
    }
}
