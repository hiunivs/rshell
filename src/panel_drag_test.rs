//! 隔离实验:egui Panel 侧栏拖拽后宽度是否保持(不依赖 RShellApp)

#[cfg(test)]
mod tests {
    use egui::containers::panel::PanelState;

    #[test]
    #[ignore]
    fn panel_drag_debug() {
        struct TestApp;
        impl eframe::App for TestApp {
            fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
                // 与真实应用一致的四面板结构
                egui::Panel::top("tab_bar").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("⌂ ＋");
                    });
                });
                egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(180.0)
                    .size_range(130.0..=320.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label("系统信息");
                                ui.label("负载 —");
                            });
                    });
                egui::Panel::bottom("file_manager")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(120.0..=600.0)
                    .show(ui, |ui| {
                        ui.label("文件 命令");
                    });
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.label("terminal");
                });
                // 把手响应状态
                let id = egui::Id::new("sidebar").with("__resize");
                if let Some(resp) = ui.ctx().read_response(id) {
                    eprintln!(
                        "  resize: rect={:?} hovered={} dragged={}",
                        resp.rect,
                        resp.hovered(),
                        resp.dragged()
                    );
                }
            }
        }
        let mut harness = egui_kittest::Harness::new_eframe(|_cc| TestApp);
        let width = |h: &egui_kittest::Harness<TestApp>| {
            PanelState::load(&h.ctx, egui::Id::new("sidebar")).map(|s| s.outer_rect.width())
        };
        harness.set_size(egui::vec2(1280.0, 800.0));
        harness.run_steps(3);
        eprintln!("初始: {:?}", width(&harness));
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
        }
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(280.0, 400.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(8);
        eprintln!("松手后: {:?}", width(&harness));
        let img = harness.render().expect("render");
        img.save("/tmp/panel_drag_debug.png").unwrap();
    }
}
