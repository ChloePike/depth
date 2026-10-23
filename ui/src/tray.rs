//! Menu bar (macOS) / notification area (Windows) item: the current coin's price and the position
//! PnL in the title (macOS) or tooltip (Windows). A left click asks for the popover (drawn by the
//! app, lib.rs), a right click shows a plain menu with every position and Open / Quit. Updated at
//! most once a second and only when the text changed. Other platforms: nothing (Linux trays need GTK).

/// What a click asks the app for. `Popover` carries the item's screen rect (physical pixels).
pub enum TrayAct { Open, Quit, Popover { x: f64, y: f64, w: f64, h: f64 } }

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::TrayAct;
    use std::time::{Duration, Instant};
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray { icon: TrayIcon, open: MenuId, quit: MenuId, shown: (String, Vec<String>), at: Instant }

    fn menu(lines: &[String]) -> (Menu, MenuId, MenuId) {
        let m = Menu::new();
        for l in lines { let _ = m.append(&MenuItem::new(l, false, None)); }
        if !lines.is_empty() { let _ = m.append(&PredefinedMenuItem::separator()); }
        let (open, quit) = (MenuItem::new(super::super::l("Open Depth"), true, None), MenuItem::new(super::super::l("Quit Depth"), true, None));
        let _ = m.append(&open);
        let _ = m.append(&quit);
        (m, open.id().clone(), quit.id().clone())
    }

    /// macOS: the status item's button gets an attributed title (tabular digits, coloured pieces).
    /// tray-icon keeps the NSStatusItem private, so the button is found in the app's own
    /// NSStatusBarWindow (Depth has one status item).
    #[cfg(target_os = "macos")]
    fn colour_title(parts: &[(String, Option<eframe::egui::Color32>)]) {
        use objc2::{rc::Retained, runtime::AnyObject, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSButton, NSColor, NSFont, NSFontAttributeName, NSFontWeightRegular, NSForegroundColorAttributeName};
        use objc2_foundation::{NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString, NSString};
        let Some(mtm) = MainThreadMarker::new() else { return };
        let font = unsafe { NSFont::monospacedDigitSystemFontOfSize_weight(0.0, NSFontWeightRegular) };
        let title = NSMutableAttributedString::new();
        for (text, col) in parts {
            let color = match col {
                Some(c) => NSColor::colorWithSRGBRed_green_blue_alpha(c.r() as f64 / 255.0, c.g() as f64 / 255.0, c.b() as f64 / 255.0, 1.0),
                None => NSColor::labelColor(),
            };
            let keys: [&NSAttributedStringKey; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
            let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> = NSDictionary::from_slices(&keys, &[font.as_ref() as &AnyObject, color.as_ref() as &AnyObject]);
            let piece = unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(text), &attrs) };
            title.appendAttributedString(&piece);
        }
        for w in NSApplication::sharedApplication(mtm).windows().iter() {
            if w.class().name().to_str() != Ok("NSStatusBarWindow") { continue; }
            if let Some(b) = w.contentView().and_then(|v| v.downcast::<NSButton>().ok()) { b.setAttributedTitle(&title); }
        }
    }

    /// The app icon at 32 px for the Windows tray (macOS shows the text only).
    #[cfg(target_os = "windows")]
    fn icon() -> Option<tray_icon::Icon> {
        let img = crate::theme::decode_png(include_bytes!("../../assets/appicon/icon-1024.png"))?;
        let (w, h, n) = (img.size[0], img.size[1], 32usize);
        let mut rgba = Vec::with_capacity(n * n * 4);
        for y in 0..n { for x in 0..n {
            let p = img.pixels[(y * h / n) * w + x * w / n];
            rgba.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
        } }
        tray_icon::Icon::from_rgba(rgba, n as u32, n as u32).ok()
    }

    impl Tray {
        pub fn new() -> Option<Self> {
            let (m, open, quit) = menu(&[]);
            let b = TrayIconBuilder::new().with_menu(Box::new(m)).with_menu_on_left_click(false).with_tooltip("Depth");
            #[cfg(target_os = "macos")]
            let b = b.with_title("Depth");
            #[cfg(target_os = "windows")]
            let b = match icon() { Some(i) => b.with_icon(i), None => b };
            Some(Tray { icon: b.build().ok()?, open, quit, shown: Default::default(), at: Instant::now() - Duration::from_secs(5) })
        }

        /// `parts`: the title in pieces, each in the label colour (None) or its own (PnL green / red).
        pub fn update(&mut self, parts: Vec<(String, Option<eframe::egui::Color32>)>, lines: Vec<String>) {
            let title: String = parts.iter().map(|p| p.0.as_str()).collect();
            if self.at.elapsed() < Duration::from_secs(1) || (title.clone(), lines.clone()) == self.shown { return; }
            self.at = Instant::now();
            #[cfg(target_os = "macos")]
            {
                self.icon.set_title(Some(&title));
                colour_title(&parts);
            }
            let _ = self.icon.set_tooltip(Some(std::iter::once(title.clone()).chain(lines.iter().cloned()).collect::<Vec<_>>().join("\n")));
            if lines != self.shown.1 {
                let (m, open, quit) = menu(&lines);
                self.icon.set_menu(Some(Box::new(m)));
                (self.open, self.quit) = (open, quit);
            }
            self.shown = (title, lines);
        }

        pub fn poll(&self) -> Option<TrayAct> {
            let mut out = None;
            while let Ok(e) = MenuEvent::receiver().try_recv() {
                if e.id == self.quit { out = Some(TrayAct::Quit); } else if e.id == self.open { out = Some(TrayAct::Open); }
            }
            while let Ok(e) = TrayIconEvent::receiver().try_recv() {
                match e {
                    // the popover on the left button's release (one toggle per click)
                    TrayIconEvent::Click { rect, button: MouseButton::Left, button_state: MouseButtonState::Up, .. } =>
                        out = Some(TrayAct::Popover { x: rect.position.x, y: rect.position.y, w: rect.size.width as f64, h: rect.size.height as f64 }),
                    TrayIconEvent::DoubleClick { .. } => out = Some(TrayAct::Open),
                    _ => {}
                }
            }
            out
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::TrayAct;
    pub struct Tray;
    impl Tray {
        pub fn new() -> Option<Self> { None }
        pub fn update(&mut self, _: Vec<(String, Option<eframe::egui::Color32>)>, _: Vec<String>) {}
        pub fn poll(&self) -> Option<TrayAct> { None }
    }
}

pub use imp::Tray;
