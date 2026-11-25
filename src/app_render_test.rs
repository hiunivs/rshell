// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 全应用离屏渲染:验证底部文件面板高度是否随文件数量变化。
//! 运行:cargo test render_app -- --ignored --nocapture

#[cfg(test)]
mod tests {
    use crate::app::RShellApp;
    use crate::session::files::FileEntry;

    fn entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            size,
            mtime: 1_752_700_000,
            permissions: if is_dir { 0o40755 } else { 0o100644 },
            uid: 0,
            gid: 0,
            owner: "root".into(),
            group: "root".into(),
            is_symlink: false,
            is_broken_symlink: false,
        }
    }

    /// 渲染连接库窗口:文件夹树 + 连接列表 + 标签 chips
    #[test]
    #[ignore]
    fn render_conn_library() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.config.folders = vec!["生产".into(), "测试".into()];
            app.config.connections = vec![
                crate::config::SavedConnection {
                    name: "root@a:22".into(),
                    host: "a.com".into(),
                    port: 22,
                    user: "root".into(),
                    alias: "主Web".into(),
                    group: "生产".into(),
                    tags: vec!["web".into(), "prod".into()],
                    ..Default::default()
                },
                crate::config::SavedConnection {
                    name: "root@b:22".into(),
                    host: "b.com".into(),
                    port: 22,
                    user: "root".into(),
                    group: "测试".into(),
                    tags: vec!["db".into()],
                    ..Default::default()
                },
            ];
            app.show_library = true;
            app.config.keys = vec![crate::config::SavedKey {
                id: "k1".into(),
                name: "work-ed25519".into(),
                path: "/tmp/k1".into(),
                fingerprint: "SHA256:abcdefghijklmnopqrstuv".into(),
            }];
            app.show_keys = true;
            app.open_lib_edit("root@a:22");
        }
        harness.run_steps(3);
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_library.png").expect("save");
        eprintln!("saved /tmp/rshell_library.png");
    }

    #[test]
    fn launcher_group_header_stays_close_to_its_cards() {
        use egui_kittest::kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        harness.state_mut().config.connections = vec![crate::config::SavedConnection {
            name: "layout-test".into(),
            alias: "布局测试连接".into(),
            host: "10.0.0.1".into(),
            user: "root".into(),
            group: "布局测试目录".into(),
            ..Default::default()
        }];
        harness.run_steps(3);

        let header = harness.get_by_label("布局测试目录").rect();
        let connection = harness.get_by_label("布局测试连接").rect();
        assert!(
            connection.top() - header.bottom() < 40.0,
            "directory header left a large gap before its cards: header={header:?}, card={connection:?}"
        );
    }

    #[test]
    fn software_update_settings_render_in_development_builds() {
        use egui_kittest::kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            RShellApp::new_with_config(cc, crate::config::AppConfig::default())
        });
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app = harness.state_mut();
            app.show_settings = true;
            app.settings_sub = Some(crate::ui::settings::SettingsPage::Updates);
        }
        harness.run_steps(3);

        assert!(harness.get_by_label("版本与更新").rect().is_positive());
        assert!(harness.get_by_label("检查更新").rect().is_positive());
    }

    #[test]
    fn about_page_exposes_required_legal_notice() {
        use egui_kittest::kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            RShellApp::new_with_config(cc, crate::config::AppConfig::default())
        });
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app = harness.state_mut();
            app.show_settings = true;
            app.settings_sub = Some(crate::ui::settings::SettingsPage::About);
        }
        harness.run_steps(3);

        assert!(harness.get_by_label("Based on RShell").rect().is_positive());
        assert!(
            harness
                .get_by_label("Copyright (C) 2026 hiunivs")
                .rect()
                .is_positive()
        );
        assert!(
            harness
                .get_by_label("https://github.com/hiunivs/rshell")
                .rect()
                .is_positive()
        );
        assert!(
            harness
                .get_by_label("查看许可证与传播条件")
                .rect()
                .is_positive()
        );
    }

    #[test]
    fn reopening_tool_viewports_requests_focus_without_closing_them() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        let app = harness.state_mut();

        app.open_settings_viewport();
        assert!(app.show_settings);
        assert!(app.focus_settings_viewport);
        app.focus_settings_viewport = false;
        app.open_settings_viewport();
        assert!(app.show_settings);
        assert!(app.focus_settings_viewport);

        app.open_connection_library_viewport();
        assert!(app.show_library);
        assert!(app.focus_library_viewport);
        app.focus_library_viewport = false;
        app.open_connection_library_viewport();
        assert!(app.show_library);
        assert!(app.focus_library_viewport);
    }

    #[test]
    fn collapsing_file_panel_expands_terminal_without_resizing_viewport() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        let ctx = harness.ctx.clone();
        harness.state_mut().open_local(&ctx);
        harness.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(170));
        harness.step();
        let viewport_size = harness.ctx.content_rect().size();

        let expanded_panel_rows = harness.state().sessions[0]
            .term
            .as_ref()
            .unwrap()
            .dims()
            .rows;
        harness.state_mut().file_manager_visible = false;
        harness.step();
        std::thread::sleep(std::time::Duration::from_millis(170));
        harness.step();
        let collapsed_panel_rows = harness.state().sessions[0]
            .term
            .as_ref()
            .unwrap()
            .dims()
            .rows;

        assert!(
            collapsed_panel_rows > expanded_panel_rows,
            "隐藏文件窗后终端行数应增加: before={expanded_panel_rows}, after={collapsed_panel_rows}"
        );
        assert_eq!(
            harness.ctx.content_rect().size(),
            viewport_size,
            "收起文件窗不应修改主窗口尺寸"
        );
    }

    /// 渲染多连接标签栏:验证自绘标签块(× 在左,选中高亮,块间间隙)
    #[test]
    #[ignore]
    fn render_tab_bar() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            app.open_local(&egui::Context::default());
            app.sessions[0].title = "root@阿里云-MeleFe".into();
            app.sessions[1].title = "root@测试机".into();
            app.active = 0;
        }
        harness.run_steps(3);
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_tabs.png").expect("save");
        eprintln!("saved /tmp/rshell_tabs.png");
    }

    /// 验证内置 UI 字体和终端字体链的基本覆盖与原始行高度量。
    #[test]
    fn glyph_coverage() {
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, _: &mut ()| {
                let _ = ui;
            },
            (),
        );
        crate::install_app_fonts(&harness.ctx, "Hack");
        harness.run_steps(1);
        assert!(
            crate::terminal_font_families()
                .iter()
                .any(|family| family == "Hack"),
            "终端字体列表必须始终包含内置 Hack"
        );
        let terminal_font =
            egui::FontId::new(13.0, egui::FontFamily::Name("rshell_terminal".into()));
        // epaint 0.35 的 `has_glyph` 会在字符和替代字符由同一个字体
        // 提供时误报 false；检查实际字宽才能覆盖真实的布局路径。
        assert!(
            harness
                .ctx
                .fonts_mut(|fonts| fonts.glyph_width(&terminal_font, 'M'))
                > 0.0,
            "终端字体族必须覆盖 ASCII"
        );
        assert!(
            harness
                .ctx
                .fonts_mut(|fonts| fonts.glyph_width(&terminal_font, '中'))
                > 0.0,
            "终端字体族必须保留 CJK 回退"
        );
        let body_font = egui::FontId::proportional(13.0);
        assert!(
            harness
                .ctx
                .fonts_mut(|fonts| fonts.glyph_width(&body_font, 'R'))
                > 0.0,
            "内置 UI 字体必须覆盖拉丁字符"
        );
        assert!(
            harness
                .ctx
                .fonts_mut(|fonts| fonts.glyph_width(&body_font, '中'))
                > 0.0,
            "内置 UI 字体必须覆盖中文"
        );

        let body_row_height = harness.ctx.fonts_mut(|fonts| fonts.row_height(&body_font));
        assert!(
            (18.0..=20.0).contains(&body_row_height),
            "内置 UI 字体 13px 应使用未缩放的原始行高，实际为 {body_row_height}"
        );
    }
    #[test]
    #[ignore]
    fn render_app_panel_height() {
        for (name, count) in [("few", 2usize), ("many", 300)] {
            let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
            // 注入假文件列表(启动默认无会话,先开一个本机会话)
            {
                let app: &mut RShellApp = harness.state_mut();
                app.open_local(&egui::Context::default());
                app.sessions[0].fm.entries = (0..count)
                    .map(|i| entry(&format!("file_{i:04}.txt"), false, 1024 * i as u64))
                    .collect();
            }
            harness.set_size(egui::vec2(1280.0, 800.0));
            harness.run_steps(3);
            let img = harness.render().expect("render");
            let path = format!("/tmp/rshell_app_{name}.png");
            img.save(&path).expect("save");
            eprintln!("saved {path}");
        }
    }

    /// 文件/命令两个页签的面板高度必须一致
    #[test]
    #[ignore]
    fn render_app_tab_height() {
        use crate::app::BottomTab;
        for (name, tab) in [("files", BottomTab::Files), ("cmds", BottomTab::Commands)] {
            let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
            harness.state_mut().bottom_tab = tab;
            harness.set_size(egui::vec2(1280.0, 800.0));
            harness.run_steps(3);
            let img = harness.render().expect("render");
            let path = format!("/tmp/rshell_tab_{name}.png");
            img.save(&path).expect("save");
            eprintln!("saved {path}");
        }
    }

    /// 拖拽侧栏边框后宽度必须保持(回归:松手弹回原宽的 bug)
    #[test]
    #[ignore]
    fn sidebar_drag_persists() {
        use egui::containers::panel::PanelState;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        harness.run_steps(3);
        let width = |h: &egui_kittest::Harness<RShellApp>| {
            PanelState::load(&h.ctx, egui::Id::new("sidebar")).map(|s| s.outer_rect.width())
        };
        eprintln!("初始: {:?}", width(&harness));
        // 从当前面板宽度计算把手位置,拖到 280
        let handle_x = width(&harness).unwrap_or(180.0);
        eprintln!("把手位置: {handle_x}");
        harness.hover_at(egui::pos2(handle_x, 400.0));
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(handle_x, 400.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        for x in [handle_x + 20.0, handle_x + 60.0, 280.0] {
            harness.event(egui::Event::PointerMoved(egui::pos2(x, 400.0)));
            eprintln!("拖到 {x}: {:?}", width(&harness));
        }
        // 打开 debug overlay,渲染"拖拽进行中"的画面(此时按住未松手)
        harness.ctx.set_debug_on_hover(true);
        harness.run_steps(2);
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_dragging_debug.png").unwrap();
        eprintln!("saved /tmp/rshell_dragging_debug.png");
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(280.0, 400.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(8);
        let w = width(&harness);
        eprintln!("松手后: {w:?}");
        assert_eq!(w, Some(280.0), "拖拽后宽度应保持为 280");
    }

    /// 向左拖过最小值:宽度必须夹在下限,拖动过程中也不许跌破
    /// (跌破内容最小宽时 egui 按内容矩形给右侧面板留位,会露出背景黑缝)
    #[test]
    #[ignore]
    fn sidebar_drag_clamps_at_min() {
        use crate::ui::sidebar::SIDEBAR_W_MIN;
        use egui::containers::panel::PanelState;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        harness.run_steps(3);
        let width = |h: &egui_kittest::Harness<RShellApp>| {
            PanelState::load(&h.ctx, egui::Id::new("sidebar")).map(|s| s.outer_rect.width())
        };
        let edge = width(&harness).unwrap();
        harness.hover_at(egui::pos2(edge, 400.0));
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(edge, 400.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        // 一路拖到极左,每步检查宽度不跌破下限
        for x in [edge - 20.0, 100.0, 40.0, 0.0] {
            harness.event(egui::Event::PointerMoved(egui::pos2(x, 400.0)));
            harness.run_steps(2);
            let w = width(&harness).unwrap();
            eprintln!("拖到 {x}: 宽 {w}");
            assert!(
                w >= SIDEBAR_W_MIN,
                "拖动中宽度 {w} 跌破下限 {SIDEBAR_W_MIN}"
            );
        }
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(0.0, 400.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(8);
        assert_eq!(width(&harness), Some(SIDEBAR_W_MIN), "松手后应停在下限");
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_sidebar_min.png").unwrap();
        eprintln!("saved /tmp/rshell_sidebar_min.png");
    }

    /// 回归:loading/错误状态不得改变底部面板高度。
    /// 旧版"加载中…"是文件表矩形外的流内控件,每帧把 Panel 内容高度顶出 ~18px,
    /// PanelState 存回内容高度 → 逐帧回弹,刷新/展开目录树时整个文件窗口
    /// 被一点点顶上去(现已改为 painter 覆盖层,不占布局空间)
    #[test]
    #[ignore]
    fn app_panel_height_when_loading() {
        use egui::containers::panel::PanelState;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
        }
        let height = |h: &egui_kittest::Harness<RShellApp>| {
            PanelState::load(&h.ctx, egui::Id::new("file_manager")).map(|s| s.outer_rect.height())
        };
        for i in 0..12 {
            // 持续保持 loading(模拟刷新期间)
            harness.state_mut().sessions[0].fm.loading = true;
            harness.step();
            let h = height(&harness);
            assert_eq!(
                h,
                Some(300.0),
                "frame {i}: loading 时面板高度应保持 300,实际 {h:?}"
            );
        }

        {
            let fm = &mut harness.state_mut().sessions[0].fm;
            fm.loading = false;
            fm.transfers = vec![crate::file_manager::Transfer::new(
                1,
                "download",
                "/tmp/混合名称-file.bin".into(),
                512,
                1024,
            )];
            fm.show_transfers_pane = true;
        }
        for (position, name) in [
            (crate::file_manager::TransferPos::Bottom, "bottom"),
            (crate::file_manager::TransferPos::Right, "right"),
        ] {
            harness.state_mut().sessions[0].fm.transfer_pos = position;
            for i in 0..12 {
                harness.step();
                let h = height(&harness);
                assert_eq!(
                    h,
                    Some(300.0),
                    "{name} transfer frame {i}: 面板高度应保持 300,实际 {h:?}"
                );
            }
        }
    }

    /// 拖动底部面板把手中间帧渲染:观察"红框闪动"来源
    #[test]
    #[ignore]
    fn render_during_panel_drag() {
        use egui::containers::panel::PanelState;
        for (name, disable_unaligned) in [("on", false), ("off", true)] {
            let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
            if disable_unaligned {
                harness
                    .ctx
                    .all_styles_mut(|s| s.debug.show_unaligned = false);
            }
            harness.set_size(egui::vec2(1280.0, 800.0));
            {
                let app: &mut RShellApp = harness.state_mut();
                app.open_local(&egui::Context::default());
                app.sessions[0].fm.entries = (0..20)
                    .map(|i| entry(&format!("file_{i:02}.txt"), false, 1024))
                    .collect();
            }
            harness.run_steps(3);
            let h = PanelState::load(&harness.ctx, egui::Id::new("file_manager"))
                .map(|s| s.outer_rect.height())
                .unwrap_or(300.0);
            let y = 800.0 - h; // 面板顶边(把手位置)
            harness.event(egui::Event::PointerButton {
                pos: egui::pos2(640.0, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            });
            for dy in [-3.0, -7.0, -11.0] {
                harness.event(egui::Event::PointerMoved(egui::pos2(640.0, y + dy)));
            }
            harness.run_steps(2);
            let img = harness.render().expect("render");
            let path = format!("/tmp/rshell_paneldrag_{name}.png");
            img.save(&path).expect("save");
            eprintln!("saved {path}");
            harness.event(egui::Event::PointerButton {
                pos: egui::pos2(640.0, y - 11.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }

    /// 连续拖动底部面板把手:逐帧渲染,观察文件表区域的瞬态绘制
    #[test]
    #[ignore]
    fn render_panel_drag_frames() {
        use egui::containers::panel::PanelState;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            app.sessions[0].fm.entries = (0..30)
                .map(|i| entry(&format!("file_{i:02}.txt"), false, 1024 * i as u64))
                .collect();
        }
        harness.run_steps(3);
        let h = PanelState::load(&harness.ctx, egui::Id::new("file_manager"))
            .map(|s| s.outer_rect.height())
            .unwrap_or(300.0);
        let y = 800.0 - h;
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(640.0, y),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        for (i, dy) in [-4.0, -9.0, -13.0, -17.0, -22.0, -26.0].iter().enumerate() {
            harness.event(egui::Event::PointerMoved(egui::pos2(640.0, y + dy)));
            let img = harness.render().expect("render");
            let path = format!("/tmp/rshell_dragframe_{i}.png");
            img.save(&path).expect("save");
        }
        eprintln!("saved /tmp/rshell_dragframe_*.png");
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(640.0, y - 26.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
    }

    /// 真实拖动底部面板把手(确认把手被抓住),逐帧渲染观察红框
    #[test]
    #[ignore]
    fn render_panel_drag_frames2() {
        use egui::containers::panel::PanelState;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            app.sessions[0].fm.tree_expanded.insert("/".into());
            app.sessions[0].fm.tree_children.insert(
                "/".into(),
                ["bin", "etc", "home", "root", "usr", "var"]
                    .map(|n| entry(n, true, 0))
                    .to_vec(),
            );
            app.sessions[0].fm.entries = (0..30)
                .map(|i| entry(&format!("file_{i:02}.txt"), false, 1024 * i as u64))
                .collect();
        }
        harness.run_steps(3);
        let h = PanelState::load(&harness.ctx, egui::Id::new("file_manager"))
            .map(|s| s.outer_rect.height())
            .unwrap_or(300.0);
        let y = 800.0 - h;
        let resize_id = egui::Id::new("file_manager").with("__resize");
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(640.0, y - 1.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(2);
        let mut y_now = y - 1.0;
        for i in 0..6 {
            y_now -= 5.0;
            harness.event(egui::Event::PointerMoved(egui::pos2(640.0, y_now)));
            harness.step();
            let dragged = harness.ctx.read_response(resize_id).map(|r| r.dragged());
            let img = harness.render().expect("render");
            img.save(format!("/tmp/rshell_drag2_{i}.png"))
                .expect("save");
            eprintln!("frame {i}: dragged={dragged:?}");
        }
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(640.0, y_now),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
    }

    /// 复现 warn_if_rect_changes_id 红框:文件表先向下滚动,再拖动面板把手,
    /// 首行可见行变化 → 单元格 unique_id 平移 → 同一矩形 id 全换 → 红色 2px 框
    #[test]
    #[ignore]
    fn render_warn_rect_changes_id() {
        use egui::containers::panel::PanelState;
        for (name, disable_warn) in [("warn_on", false), ("warn_off", true)] {
            let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
            if disable_warn {
                harness
                    .ctx
                    .all_styles_mut(|s| s.debug.warn_if_rect_changes_id = false);
            }
            harness.set_size(egui::vec2(1280.0, 800.0));
            {
                let app: &mut RShellApp = harness.state_mut();
                app.open_local(&egui::Context::default());
                app.sessions[0].fm.entries = (0..60)
                    .map(|i| entry(&format!("file_{i:02}.txt"), false, 1024 * i as u64))
                    .collect();
            }
            harness.run_steps(3);
            // 先把文件表向下滚动几行(让首行可见行 > 0);指针须在表格上
            harness.event(egui::Event::PointerMoved(egui::pos2(700.0, 700.0)));
            harness.step();
            for _ in 0..3 {
                harness.event(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, -3.0),
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                });
                harness.step();
            }
            harness.run_steps(2);
            // 拖动底部面板把手
            let h = PanelState::load(&harness.ctx, egui::Id::new("file_manager"))
                .map(|s| s.outer_rect.height())
                .unwrap_or(300.0);
            let y = 800.0 - h;
            harness.event(egui::Event::PointerButton {
                pos: egui::pos2(640.0, y - 1.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            });
            harness.run_steps(2);
            let mut y_now = y - 1.0;
            for i in 0..3 {
                y_now -= 8.0;
                harness.event(egui::Event::PointerMoved(egui::pos2(640.0, y_now)));
                harness.step();
                let img = harness.render().expect("render");
                img.save(format!("/tmp/rshell_warnid_{name}_{i}.png"))
                    .expect("save");
            }
            eprintln!("saved /tmp/rshell_warnid_{name}_*.png");
            harness.event(egui::Event::PointerButton {
                pos: egui::pos2(640.0, y_now),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            harness.run_steps(2);
        }
    }

    /// 验证 show_unaligned 机理与开关:侧栏宽设为小数 → 表格单元格坐标小数化。
    /// 用 RSHELL_DEBUG_SHAPES=1 --nocapture 跑,观察 app 钩子打印的橙色 Line 数量
    /// (disable=false 应有一批,disable=true 应为零)
    #[test]
    #[ignore]
    fn unaligned_lines_mechanism() {
        use egui::containers::panel::PanelState;
        for disable in [false, true] {
            let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
            if disable {
                harness
                    .ctx
                    .all_styles_mut(|s| s.debug.show_unaligned = false);
            }
            harness.set_size(egui::vec2(1280.0, 800.0));
            {
                let app: &mut RShellApp = harness.state_mut();
                app.open_local(&egui::Context::default());
                app.sessions[0].fm.entries = (0..10)
                    .map(|i| entry(&format!("file_{i}.txt"), false, 1024))
                    .collect();
            }
            harness.run_steps(3);
            // 侧栏宽设为小数,模拟拖动产生的小数坐标
            let mut st = PanelState::load(&harness.ctx, egui::Id::new("sidebar")).unwrap();
            st.outer_rect.max.x = 233.83;
            harness
                .ctx
                .data_mut(|d| d.insert_persisted(egui::Id::new("sidebar"), st));
            eprintln!("=== disable={disable} 下一帧 ===");
            harness.step();
        }
    }

    /// 断开后终端内容保留 + 标记线;重连后内容继续追加
    #[test]
    #[ignore]
    fn render_disconnect_reconnect() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
        }
        harness.run_steps(4);
        std::thread::sleep(std::time::Duration::from_millis(800));
        harness.run_steps(2);
        harness.state_mut().sessions[0].disconnect();
        harness.run_steps(2);
        harness
            .render()
            .expect("render")
            .save("/tmp/rshell_disconnected.png")
            .expect("save");
        harness.state_mut().sessions[0].reconnect(&egui::Context::default());
        std::thread::sleep(std::time::Duration::from_millis(800));
        harness.run_steps(4);
        harness
            .render()
            .expect("render")
            .save("/tmp/rshell_reconnected.png")
            .expect("save");
        eprintln!("saved /tmp/rshell_disconnected.png /tmp/rshell_reconnected.png");
    }

    /// 侧栏内存行:大数值文本(21.3G/78.3G)不应溢出/截断
    #[test]
    #[ignore]
    fn render_usage_row_wide_text() {
        use crate::session::monitor::SystemMetrics;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            let m = SystemMetrics {
                cpu_percent: 42.0,
                mem_total_kb: 78 * 1024 * 1024 + 300_000,
                mem_used_kb: 21 * 1024 * 1024 + 300_000,
                swap_total_kb: 4 * 1024 * 1024,
                swap_used_kb: 0,
                ..Default::default()
            };
            app.sessions[0].monitor.update(m.into());
        }
        harness.run_steps(3);
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_usage_row.png").expect("save");
        eprintln!("saved /tmp/rshell_usage_row.png");
    }

    /// 渲染拖拽悬停遮罩:注入 hovered_files + 指针在文件面板上
    #[test]
    #[ignore]
    fn render_drag_drop_hover() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
        }
        harness.run_steps(3);
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(640.0, 700.0)));
        harness.input_mut().hovered_files.push(egui::HoveredFile {
            path: Some(std::path::PathBuf::from("/tmp/x.txt")),
            ..Default::default()
        });
        harness.step();
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_drag_hover.png").expect("save");
        eprintln!("saved /tmp/rshell_drag_hover.png");
    }

    /// 拖拽上传:注入 DroppedFile 事件,文件应被复制到当前目录
    #[test]
    #[ignore]
    fn drag_drop_upload() {
        let tmp = std::env::temp_dir().join(format!("rshell_dd_{}", std::process::id()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        let src = src_dir.join("drag.txt");
        std::fs::write(&src, b"drag me").unwrap();

        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            // 开一个本机会话,当前目录设为目标目录
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            app.sessions[0].fm.cwd = dst_dir.to_string_lossy().into_owned();
        }
        harness.run_steps(2);
        // 模拟把文件拖入底部面板中央并落下
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(640.0, 700.0)));
        harness.input_mut().dropped_files.push(egui::DroppedFile {
            path: Some(src.clone()),
            ..Default::default()
        });
        harness.step();
        // 本机文件服务是真实线程,等拷贝完成
        std::thread::sleep(std::time::Duration::from_millis(500));
        harness.run_steps(3);

        assert!(
            dst_dir.join("drag.txt").exists(),
            "dropped file should be uploaded (copied) into cwd"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 拖拽上传:即使 pointer 位置缺失(没发 PointerMoved),落下也应上传。
    /// 回归:旧版用 over_panel 守卫,pointer 为 None 时丢掉的文件被直接 return。
    #[test]
    #[ignore]
    fn drag_drop_upload_without_pointer_pos() {
        let tmp = std::env::temp_dir().join(format!("rshell_ddnp_{}", std::process::id()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        let src = src_dir.join("drag.txt");
        std::fs::write(&src, b"drag me").unwrap();

        let mut harness = egui_kittest::Harness::new_eframe(|cc| RShellApp::new(cc));
        harness.set_size(egui::vec2(1280.0, 800.0));
        {
            let app: &mut RShellApp = harness.state_mut();
            app.open_local(&egui::Context::default());
            app.sessions[0].fm.cwd = dst_dir.to_string_lossy().into_owned();
        }
        harness.run_steps(2);
        // 注意:不发 PointerMoved,模拟 drop 事件到达时 pointer.latest_pos() 为 None
        harness.input_mut().dropped_files.push(egui::DroppedFile {
            path: Some(src.clone()),
            ..Default::default()
        });
        harness.step();
        std::thread::sleep(std::time::Duration::from_millis(500));
        harness.run_steps(3);

        assert!(
            dst_dir.join("drag.txt").exists(),
            "pointer 位置缺失也应上传到 cwd"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
