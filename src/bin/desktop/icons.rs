//! Small, original vector icons. Paths use a 24-point grid and scale with DPI.
use super::*;
use egui::{pos2, Align2, FontId, Rect, Sense, Shape};

#[derive(Clone, Copy)]
pub enum Icon {
    Spark,
    Chat,
    Grid,
    Settings,
    Folder,
    Code,
    Search,
    Arrow,
    Shield,
    Link,
    Terminal,
    Branch,
    File,
}

pub fn draw(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let at = |x: f32, y: f32| {
        pos2(
            rect.left() + x * rect.width() / 24.0,
            rect.top() + y * rect.height() / 24.0,
        )
    };
    let stroke = Stroke::new((rect.width() / 24.0 * 1.65).max(1.2), color);
    let line = |points: &[(f32, f32)]| {
        p.add(Shape::line(
            points.iter().map(|&(x, y)| at(x, y)).collect(),
            stroke,
        ))
    };
    match icon {
        Icon::Spark => {
            line(&[
                (12., 2.),
                (14.8, 9.2),
                (22., 12.),
                (14.8, 14.8),
                (12., 22.),
                (9.2, 14.8),
                (2., 12.),
                (9.2, 9.2),
                (12., 2.),
            ]);
        }
        Icon::Chat => {
            line(&[
                (5., 4.),
                (19., 4.),
                (21., 6.),
                (21., 16.),
                (19., 18.),
                (10., 18.),
                (5., 22.),
                (5., 18.),
                (3., 16.),
                (3., 6.),
                (5., 4.),
            ]);
            line(&[(7., 9.), (17., 9.)]);
            line(&[(7., 13.), (14., 13.)]);
        }
        Icon::Grid => {
            for (x, y) in [(3., 3.), (14., 3.), (3., 14.), (14., 14.)] {
                p.rect_stroke(
                    Rect::from_min_max(at(x, y), at(x + 7., y + 7.)),
                    2,
                    stroke,
                    egui::StrokeKind::Inside,
                );
            }
        }
        Icon::Settings => {
            for (y, x) in [(6., 8.), (12., 16.), (18., 9.)] {
                line(&[(3., y), (21., y)]);
                p.circle_filled(at(x, y), 2.5 * rect.width() / 24., PANEL);
                p.circle_stroke(at(x, y), 2.5 * rect.width() / 24., stroke);
            }
        }
        Icon::Folder => {
            line(&[
                (3., 7.),
                (3., 19.),
                (21., 19.),
                (21., 7.),
                (12., 7.),
                (10., 4.),
                (3., 4.),
                (3., 7.),
            ]);
        }
        Icon::Code => {
            line(&[(8., 6.), (2., 12.), (8., 18.)]);
            line(&[(16., 6.), (22., 12.), (16., 18.)]);
            line(&[(14., 3.), (10., 21.)]);
        }
        Icon::Search => {
            p.circle_stroke(at(10., 10.), 7. * rect.width() / 24., stroke);
            line(&[(15., 15.), (22., 22.)]);
        }
        Icon::Arrow => {
            line(&[(5., 12.), (19., 12.)]);
            line(&[(13., 6.), (19., 12.), (13., 18.)]);
        }
        Icon::Shield => {
            line(&[
                (12., 2.),
                (21., 6.),
                (20., 15.),
                (17., 19.),
                (12., 22.),
                (7., 19.),
                (4., 15.),
                (3., 6.),
                (12., 2.),
            ]);
            line(&[(8., 12.), (11., 15.), (16., 9.)]);
        }
        Icon::Link => {
            p.circle_stroke(at(5., 12.), 2. * rect.width() / 24., stroke);
            p.circle_stroke(at(19., 5.), 2. * rect.width() / 24., stroke);
            p.circle_stroke(at(19., 19.), 2. * rect.width() / 24., stroke);
            line(&[(7., 12.), (12., 12.), (16., 5.)]);
            line(&[(12., 12.), (16., 19.)]);
        }
        Icon::Terminal => {
            p.rect_stroke(
                Rect::from_min_max(at(2., 4.), at(22., 20.)),
                3,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(&[(6., 8.), (10., 12.), (6., 16.)]);
            line(&[(13., 16.), (18., 16.)]);
        }
        Icon::Branch => {
            line(&[(6., 7.), (6., 17.)]);
            line(&[(6., 14.), (16., 14.), (18., 12.), (18., 7.)]);
            for (x, y) in [(6., 4.), (6., 20.), (18., 4.)] {
                p.circle_stroke(at(x, y), 2.5 * rect.width() / 24., stroke);
            }
        }
        Icon::File => {
            line(&[
                (5., 2.),
                (14., 2.),
                (20., 8.),
                (20., 22.),
                (5., 22.),
                (5., 2.),
            ]);
            line(&[(14., 2.), (14., 8.), (20., 8.)]);
            line(&[(9., 13.), (16., 13.)]);
            line(&[(9., 17.), (15., 17.)]);
        }
    }
}

pub fn glyph(ui: &mut Ui, icon: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    draw(ui, rect, icon, color);
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, width: f32, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 36.), Sense::click());
    let fill = if active {
        Color32::from_rgb(38, 59, 54)
    } else if response.hovered() {
        CARD
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 9, fill);
    let color = if active { ACCENT } else { MUTED };
    draw(
        ui,
        Rect::from_min_size(rect.min + Vec2::new(11., 9.), Vec2::splat(18.)),
        icon,
        color,
    );
    ui.painter().text(
        rect.min + Vec2::new(38., 18.),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.),
        if active { TEXT } else { MUTED },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response
}

pub fn brand(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, (size * 0.28) as u8, ACCENT);
    let points = [
        (0.21, 0.34),
        (0.35, 0.69),
        (0.50, 0.38),
        (0.65, 0.69),
        (0.79, 0.34),
    ]
    .map(|(x, y)| rect.min + Vec2::new(x * size, y * size));
    ui.painter()
        .add(Shape::line(points.to_vec(), Stroke::new(size * 0.07, BG)));
}
