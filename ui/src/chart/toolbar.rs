//! The chart toolbar of the plain egui host: intervals, source, indicators, layout, alerts,
//! replay and the drawing tools.
use super::*;
use super::chrome::{chevron_down, layout_glyph, menu_head, menu_row, menu_rows, menu_sep, pill, tb_button, tool_button, vsep, Glyph, Tb};

impl Chart {
    pub(super) fn toolbar(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str) {
        // design: a 36 px bar, controls in its top 28 px, 8 px clear of the hairline; compact interval pills, outlined menus, tools on a strip
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
        ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, line()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(bar.min, pos2(bar.right(), bar.bottom() - 8.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 2.0;
        for (m, l) in &TFS[..TF_QUICK] {
            if pill(ui, l, self.tf == *m).clicked() { self.tf = *m; }
        }
        // a menu interval in use shows as a selected pill after the quick ones
        if let Some((_, l)) = TFS[TF_QUICK..].iter().find(|(m, _)| *m == self.tf) { pill(ui, l, true); }
        let more = pill(ui, "", false);
        chevron_down(ui.painter(), more.rect.center(), mu());
        egui::Popup::menu(&more).show(|ui| {
            menu_rows(ui, 120.0);
            for (m, l) in &TFS[TF_QUICK..] { if menu_row(ui, self.tf == *m, l, None).clicked() { self.tf = *m; ui.close(); } }
        });
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(6.0);
        vsep(ui);
        let mut venues: Vec<Exchange> = a.series.keys().filter(|(e, m)| *m == market && e.is_some()).filter_map(|(e, _)| *e).collect();
        venues.sort_by_key(|e| format!("{e:?}"));
        let cur = self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| t("src.agg").to_string());
        let src = tb_button(ui, &cur, Tb::Outline, 0, true);
        egui::Popup::menu(&src).show(|ui| {
            menu_rows(ui, 180.0);
            menu_head(ui, super::l("Source"));
            if menu_row(ui, self.src.is_none(), t("src.agg"), None).clicked() { self.src = None; ui.close(); }
            for e in venues { if menu_row(ui, self.src == Some(e), &format!("{e:?}"), None).clicked() { self.src = Some(e); ui.close(); } }
        });
        self.indicators_menu(ui, market);
        self.layout_menu(ui);
        self.alerts_menu(ui, base);
        if tb_button(ui, t("replay.title"), if self.replay.is_some() { Tb::Tint } else { Tb::Ghost }, 0, false).on_hover_text(super::l("Bar replay: pick a bar, then play forward")).clicked() {
            self.replay = if self.replay.is_some() { None } else { Some(Replay::new(None, 0.0)) };
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // the drawing tools on one subtle strip; the everyday ones as buttons, the rest behind "⋯"
            const QUICK: [(Tool, &str); 5] = [(Tool::Rect, "draw.rect"), (Tool::Fib, "draw.fib"), (Tool::Ray, "draw.ray"), (Tool::Trend, "draw.trend"), (Tool::HLine, "draw.hline")];
            const MORE: [(Tool, &str); 4] = [(Tool::Channel, "draw.channel"), (Tool::Text, "draw.text"), (Tool::Measure, "draw.measure"), (Tool::Position, "draw.position")];
            let (quick, more) = (&QUICK, MORE.to_vec());
            let in_more = more.iter().find(|(k, _)| self.tool == Some(*k)).map(|x| x.0);
            let n = self.drawings.get(base).map_or(0, |v| v.len());
            egui::Frame::new().fill(panel2()).corner_radius(5).inner_margin(2).show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if n > 0 {
                    if tool_button(ui, Glyph::Trash, false).on_hover_text(format!("{} ({n})", t("draw.clear"))).clicked() { self.drawings.remove(base); }
                    if tool_button(ui, Glyph::Eye(self.hide), self.hide).on_hover_text(super::l("Hide drawings")).clicked() { self.hide = !self.hide; }
                    let (r, _) = ui.allocate_exact_size(vec2(5.0, 16.0), Sense::hover());
                    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, line()));
                }
                // the "more" button shows the armed tool from its menu
                let mb = tool_button(ui, in_more.map_or(Glyph::More, Glyph::Tool), in_more.is_some()).on_hover_text(super::l("More drawing tools"));
                egui::Popup::menu(&mb).show(|ui| {
                    menu_rows(ui, 180.0);
                    for &(tool, key) in &more {
                        let on = self.tool == Some(tool);
                        if menu_row(ui, on, t(key), Some(Glyph::Tool(tool))).clicked() { self.tool = if on { None } else { Some(tool) }; self.pending.clear(); ui.close(); }
                    }
                });
                for &(tool, key) in quick {
                    if tool_button(ui, Glyph::Tool(tool), self.tool == Some(tool)).on_hover_text(t(key)).clicked() {
                        self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
                        self.pending.clear();
                    }
                }
            });
            if (self.right.abs() > 0.5 || self.bar_w != 7.0 || self.y_range.is_some()) && tb_button(ui, t("chart.reset"), Tb::Ghost, 0, false).on_hover_text(super::l("Back to the live edge and default zoom")).clicked() {
                self.right = 0.0; self.bar_w = 7.0; self.y_range = None;
            }
        });
    }

    /// Sub-panes, overlays (with their inputs) and levels in one menu.
    fn indicators_menu(&mut self, ui: &mut Ui, market: Market) {
        let perp = market == Market::Perp;
        let n_on = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || perp)).count()
            + [self.heat, self.ma, self.bb, self.liqmap && perp, self.sr, self.breakouts, self.vwap, self.cone, self.walls, self.mark && perp].iter().filter(|x| **x).count();
        let resp = tb_button(ui, t("chart.indicators"), Tb::Outline, n_on, false);
        egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            menu_rows(ui, 260.0);
            let indent = |ui: &mut Ui| ui.add_space(26.0);
            menu_head(ui, super::l("Panes"));
            for (p, on) in self.panes.iter_mut() {
                if p.perp_only() && !perp { continue; }
                ui.checkbox(on, t(p.key()));
            }
            ui.horizontal(|ui| {
                indent(ui);
                ui.label(RichText::new("RSI").font(prop(12.0)).color(mu()));
                ui.add(egui::DragValue::new(&mut self.ind.rsi).range(1..=500).speed(0.3));
                ui.add_space(6.0);
                ui.label(RichText::new("MACD").font(prop(12.0)).color(mu()));
                for x in self.ind.macd.iter_mut() { ui.add(egui::DragValue::new(x).range(1..=500).speed(0.3)); }
            });
            menu_sep(ui);
            menu_head(ui, super::l("Overlays"));
            ui.checkbox(&mut self.ma, super::l("Moving averages"));
            if self.ma {
                for m in self.mas.iter_mut() {
                    ui.horizontal(|ui| {
                        indent(ui);
                        ui.checkbox(&mut m.on, "");
                        let mut c = rgb(m.col);
                        if ui.color_edit_button_srgba(&mut c).changed() { m.col = super::theme::pack(c); }
                        ui.add(egui::DragValue::new(&mut m.n).range(1..=1000).speed(0.3));
                        if ui.selectable_label(!m.ema, "SMA").clicked() { m.ema = false; }
                        if ui.selectable_label(m.ema, "EMA").clicked() { m.ema = true; }
                    });
                }
            }
            ui.checkbox(&mut self.bb, super::l("Bollinger bands"));
            if self.bb {
                ui.horizontal(|ui| {
                    indent(ui);
                    ui.add(egui::DragValue::new(&mut self.ind.bb_n).range(1..=500).speed(0.3));
                    ui.add(egui::DragValue::new(&mut self.ind.bb_k).range(0.1..=10.0).speed(0.05).fixed_decimals(1).suffix(" σ"));
                });
            }
            ui.checkbox(&mut self.fp, super::l("Footprint (traded value per price)")).on_hover_text(super::l("Live trades since Depth started; zoom in to read the numbers"));
            if self.fp {
                ui.horizontal(|ui| {
                    indent(ui);
                    ui.selectable_value(&mut self.fp_style, 0, super::l("Cluster bars"));
                    ui.selectable_value(&mut self.fp_style, 1, super::l("Side clusters"));
                });
            }
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.whales, super::l("Whale trades"));
                ui.add_enabled(self.whales, egui::DragValue::new(&mut self.whale_usd).range(10_000.0..=50_000_000.0).speed(5_000.0).prefix("≥ $").custom_formatter(|v, _| fmt_usd_short(v)));
            });
            if perp { ui.checkbox(&mut self.fills_on, super::l("My fills (B / S)")); }
            ui.checkbox(&mut self.algos, t("chart.algos")).on_hover_text(t("chart.algos_tip"));
            ui.checkbox(&mut self.heat, super::l("Book heatmap"));
            if perp { ui.checkbox(&mut self.liqmap, super::l("Liquidation map")).on_hover_text(t("chart.liqmap_tip")); }
            menu_sep(ui);
            menu_head(ui, super::l("Levels"));
            ui.checkbox(&mut self.sr, super::l("Volume profile (POC · value area · HVN)"));
            ui.checkbox(&mut self.breakouts, super::l("Breakouts (closes through VAH / VAL / HVN)"));
            ui.checkbox(&mut self.vwap, super::l("Session VWAP ±1σ ±2σ"));
            ui.checkbox(&mut self.cone, super::l("Expected move cone (volatility model)"));
            ui.checkbox(&mut self.walls, super::l("Liquidity walls"));
            if perp { ui.checkbox(&mut self.mark, super::l("Mark price")); }
            menu_sep(ui);
            ui.checkbox(&mut self.magnet, super::l("Magnet crosshair"));
        });
    }

    /// Chart layout (the app owns the cells: the choice goes out through `layout_req`).
    fn layout_menu(&mut self, ui: &mut Ui) {
        const LAYOUTS: [(&str, &str); 5] = [("1", "One chart"), ("2", "Two side by side"), ("2v", "Two stacked"), ("3", "Three"), ("4", "Four")];
        let layout = self.layout.clone();
        let resp = kit::icon_button(ui, kit::H, false, |p, r, col| layout_glyph(p, Rect::from_center_size(r.center(), vec2(16.0, 12.0)), &layout, col));
        egui::Popup::menu(&resp.on_hover_text(super::l("Chart layout: the toolbar drives the highlighted chart"))).show(|ui| {
            menu_rows(ui, 200.0);
            menu_head(ui, super::l("Layout"));
            for (id, name) in LAYOUTS {
                if menu_row(ui, self.layout == id, super::l(name), Some(Glyph::Layout(id))).clicked() { self.layout_req = Some(id.to_string()); ui.close(); }
            }
        });
    }

    /// Armed alerts on every symbol (sound, remove) and the sound of new ones.
    fn alerts_menu(&mut self, ui: &mut Ui, base: &str) {
        let mut rows: Vec<(String, usize, f64, String)> = self.drawings.iter().flat_map(|(b, l)| l.iter().enumerate().filter_map(move |(i, d)| {
            Some((b.clone(), i, d.level_at(depth::now_ms())?, d.alert.clone()?))
        })).collect();
        rows.sort_by(|x, y| (x.0 != base, &x.0).cmp(&(y.0 != base, &y.0)));
        let resp = tb_button(ui, super::l("Alerts"), if rows.is_empty() { Tb::Ghost } else { Tb::Tint }, rows.len(), false).on_hover_text(super::l("Price alerts"));
        egui::Popup::menu(&resp).show(|ui| {
            menu_rows(ui, 260.0);
            menu_head(ui, super::l("Price alerts"));
            if rows.is_empty() {
                ui.horizontal(|ui| { ui.add_space(8.0); ui.label(RichText::new(super::l("Right-click the chart or a line to add an alert")).font(prop(12.0)).color(dim())); });
            }
            for (b, i, px, snd) in rows {
                ui.menu_button(format!("{b}  {}  ·  {}", fmt_px(px), sound_name(&snd)), |ui| {
                    menu_rows(ui, 160.0);
                    if let Some(d) = self.drawings.get_mut(&b).and_then(|l| l.get_mut(i)) {
                        if let Some(s) = d.alert.as_mut() {
                            for (id, name) in sounds() { if menu_row(ui, *s == id, &name, None).clicked() { play_sound(&id); *s = id; } }
                        }
                        menu_sep(ui);
                        if ui.button(RichText::new(super::l("Remove alert")).color(dn())).clicked() { d.alert = None; ui.close(); }
                    }
                });
            }
            menu_sep(ui);
            ui.menu_button(super::l("Sound of new alerts"), |ui| {
                menu_rows(ui, 160.0);
                for (id, name) in sounds() { if menu_row(ui, self.alert_sound == id, &name, None).clicked() { play_sound(&id); self.alert_sound = id; } }
            });
        });
    }
}
