//! Small shared pieces: buttons, the switch, the avatar, context menus,
//! toasts and the confirmation dialog.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Bounds, ClickEvent, Corner, CursorStyle,
    ElementId, FontWeight, HitboxBehavior, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, Pixels, Point, SharedString, Transformation, Window, anchored, canvas,
    deferred, div, img, percentage, prelude::*, px,
};

use crate::anim;
use crate::assets::icon;
use crate::theme;

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

// ---- Buttons ----------------------------------------------------------------

/// Square icon button: color-only hover.
pub fn icon_button(
    id: impl Into<ElementId>,
    glyph: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(theme::CONTROL_HEIGHT)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_CONTROL)
        .cursor_pointer()
        .hover(|el| el.bg(theme::hover()))
        .on_click(on_click)
        .child(icon(glyph, px(18.), theme::text_title()))
}

/// Secondary button: subtle fill, brighter on hover.
pub fn text_button(
    id: impl Into<ElementId>,
    glyph: Option<&'static str>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(theme::CONTROL_HEIGHT)
        .px(px(12.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded(theme::RADIUS_CONTROL)
        .cursor_pointer()
        .bg(theme::fill())
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::text_title())
        .hover(|el| el.bg(theme::fill_hover()).text_color(theme::text()))
        .on_click(on_click)
        .when_some(glyph, |el, glyph| {
            el.child(icon(glyph, px(15.), theme::text_title()))
        })
        .child(label.into())
}

/// The one filled button of a dialog: accent, or red for something destructive. (Its hover
/// is set here once: GPUI panics if an element's hover is set twice.)
fn filled_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    destructive: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, hover) = if destructive {
        (theme::danger(), theme::danger_hover())
    } else {
        (theme::accent(), theme::accent_hover())
    };
    div()
        .id(id)
        .h(theme::CONTROL_HEIGHT)
        .px(px(14.))
        .flex_none()
        .flex()
        .items_center()
        .rounded(theme::RADIUS_CONTROL)
        .cursor_pointer()
        .bg(bg)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui::white())
        .hover(move |el| el.bg(hover))
        .on_click(on_click)
        .child(label.into())
}

pub fn link(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .text_color(theme::accent_text())
        .cursor_pointer()
        .hover(|el| el.text_color(theme::text()))
        .on_click(on_click)
        .child(label.into())
}

/// An on/off switch. With `animate`, the knob slides in from the other side
/// (pass it for the switch that was just flipped).
pub fn switch(
    id: &'static str,
    on: bool,
    animate: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    const OFF_X: f32 = 2.;
    const ON_X: f32 = 18.;
    let (from, to) = if on { (OFF_X, ON_X) } else { (ON_X, OFF_X) };
    let knob = div()
        .absolute()
        .top(px(2.))
        .size(px(16.))
        .rounded_full()
        .bg(gpui::white());
    div()
        .id(id)
        .relative()
        .flex_none()
        .w(px(36.))
        .h(px(20.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on {
            theme::accent()
        } else {
            theme::fill_hover()
        })
        .on_click(on_click)
        .child(if animate {
            knob.with_animation(
                ElementId::NamedInteger(id.into(), on as u64),
                anim::ease(anim::NORMAL),
                move |el, t| el.left(px(from + (to - from) * t)),
            )
            .into_any_element()
        } else {
            knob.left(px(to)).into_any_element()
        })
}

// ---- Avatar -----------------------------------------------------------------

/// The account picture, or the account's initials when there isn't one.
pub fn avatar(picture: Option<Arc<Path>>, name: &str, size: Pixels) -> impl IntoElement {
    let base = div()
        .size(size)
        .flex_none()
        .rounded_full()
        .overflow_hidden()
        .bg(theme::accent_soft());
    match picture {
        Some(path) => base.child(
            img(path)
                .size_full()
                .rounded_full()
                .object_fit(ObjectFit::Cover),
        ),
        None => base
            .flex()
            .items_center()
            .justify_center()
            .text_size(size * 0.4)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::accent_text())
            .child(initials(name)),
    }
}

fn initials(name: &str) -> String {
    let mut words = name.split_whitespace().filter_map(|w| w.chars().next());
    let first = words.next();
    let last = words.next_back();
    first
        .into_iter()
        .chain(last)
        .flat_map(char::to_uppercase)
        .collect()
}

// ---- Spinner ----------------------------------------------------------------

/// A turning arc: something is loading. Still when motion is reduced.
pub fn spinner(id: impl Into<ElementId>, size: Pixels, color: Hsla) -> gpui::AnyElement {
    let arc = icon("spinner", size, color);
    if anim::reduced() {
        return arc.into_any_element();
    }
    arc.with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(800)).repeat(),
        |svg, t| svg.with_transformation(Transformation::rotate(percentage(t))),
    )
    .into_any_element()
}

/// A spinner and a short text, side by side: a loading status.
pub fn loading_status(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    color: Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(7.))
        .child(spinner(id, px(13.), color))
        .child(text.into())
}

// ---- Segmented control ------------------------------------------------------

/// A row of choices, one picked: a pill inside a pill.
pub fn segmented(
    id: &'static str,
    labels: &[&'static str],
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_select = Rc::new(on_select);
    div()
        .flex()
        .flex_none()
        .items_center()
        .p(px(3.))
        .gap(px(2.))
        .rounded_full()
        .bg(theme::fill())
        .children(labels.iter().enumerate().map(|(i, label)| {
            let on_select = on_select.clone();
            let chosen = i == selected;
            div()
                .id(ElementId::NamedInteger(id.into(), i as u64))
                .h(px(26.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .map(|el| {
                    if chosen {
                        el.bg(theme::popover())
                            .shadow_sm()
                            .text_color(theme::text())
                    } else {
                        el.text_color(theme::text_muted())
                            .hover(|el| el.text_color(theme::text()))
                    }
                })
                .on_click(move |_, window, cx| on_select(i, window, cx))
                .child(*label)
        }))
}

// ---- Tooltip ----------------------------------------------------------------

struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(8.))
            .bg(theme::popover())
            .shadow_md()
            .font_family(theme::FONT)
            .text_size(px(12.))
            .text_color(theme::text_title())
            .child(self.0.clone())
    }
}

/// For `.tooltip(...)`: a small label that shows on hover.
pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
}

// ---- Slider -----------------------------------------------------------------

type SliderHandler = Rc<dyn Fn(f32, &mut Window, &mut App)>;

/// A horizontal slider over 0..=1: click or drag anywhere on it.
#[derive(IntoElement)]
pub struct Slider {
    id: ElementId,
    value: f32,
    width: Pixels,
    on_change: SliderHandler,
}

pub fn slider(
    id: impl Into<ElementId>,
    value: f32,
    width: Pixels,
    on_change: impl Fn(f32, &mut Window, &mut App) + 'static,
) -> Slider {
    Slider {
        id: id.into(),
        value,
        width,
        on_change: Rc::new(on_change),
    }
}

const SLIDER_KNOB: f32 = 14.;

/// Where along the slider a pointer is, 0..=1.
fn slider_fraction(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
    let travel = f32::from(bounds.size.width) - SLIDER_KNOB;
    if travel <= 0. {
        return 0.;
    }
    ((f32::from(x - bounds.left()) - SLIDER_KNOB / 2.) / travel).clamp(0., 1.)
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Whether this slider is being dragged, kept between frames.
        let dragging = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        let value = if self.value.is_finite() {
            self.value.clamp(0., 1.)
        } else {
            0.
        };
        let travel = f32::from(self.width) - SLIDER_KNOB;
        let knob_x = value * travel;
        let on_change = self.on_change;
        let held = *dragging.read(cx);

        let events = canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                let (down_state, down_change) = (dragging.clone(), on_change.clone());
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase.bubble()
                        && event.button == MouseButton::Left
                        && hitbox.is_hovered(window)
                    {
                        down_state.update(cx, |held, cx| {
                            *held = true;
                            cx.notify();
                        });
                        down_change(slider_fraction(bounds, event.position.x), window, cx);
                        cx.stop_propagation();
                    }
                });
                let (move_state, move_change) = (dragging.clone(), on_change.clone());
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase.bubble() && *move_state.read(cx) {
                        if event.dragging() {
                            move_change(slider_fraction(bounds, event.position.x), window, cx);
                        } else {
                            move_state.update(cx, |held, cx| {
                                *held = false;
                                cx.notify();
                            });
                        }
                    }
                });
                let up_state = dragging.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase.bubble() && *up_state.read(cx) {
                        up_state.update(cx, |held, cx| {
                            *held = false;
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        div()
            .id(self.id)
            .relative()
            .w(self.width)
            .h(px(20.))
            .flex_none()
            // The track, filled up to the knob.
            .child(
                div()
                    .absolute()
                    .left(px(SLIDER_KNOB / 2.))
                    .right(px(SLIDER_KNOB / 2.))
                    .top(px(8.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme::fill_hover()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(SLIDER_KNOB / 2.))
                    .top(px(8.))
                    .w(px(knob_x))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme::accent()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(knob_x))
                    .top(px(3.))
                    .size(px(SLIDER_KNOB))
                    .rounded_full()
                    // White in either theme, like a system slider's knob.
                    .bg(if held {
                        gpui::rgb(0xe5e5ea).into()
                    } else {
                        gpui::white()
                    })
                    .shadow_md(),
            )
            .child(events)
    }
}

// ---- Context menu -----------------------------------------------------------

pub struct MenuItem {
    icon: &'static str,
    label: SharedString,
    hint: Option<&'static str>,
    separated: bool,
    run: Handler,
}

impl MenuItem {
    pub fn new(
        icon: &'static str,
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            icon,
            label: label.into(),
            hint: None,
            separated: false,
            run: Rc::new(run),
        }
    }

    /// The keyboard shortcut, shown on the right.
    pub fn hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    /// Draw a line above this item.
    pub fn separated(mut self) -> Self {
        self.separated = true;
        self
    }
}

/// A menu at `position` (window coordinates), kept inside the window.
/// `serial` identifies this opening, so each one fades in.
pub fn context_menu(
    serial: u64,
    position: Point<Pixels>,
    items: Vec<MenuItem>,
    dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    popup_menu(serial, position, Corner::TopLeft, None, items, dismiss)
}

/// A menu with that `corner` at `position`, and something above its items
/// (the account menu's name and picture).
pub fn popup_menu(
    serial: u64,
    position: Point<Pixels>,
    corner: Corner,
    header: Option<AnyElement>,
    items: Vec<MenuItem>,
    dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let dismiss: Handler = Rc::new(dismiss);
    let dismiss_out = dismiss.clone();
    let rows = items.into_iter().enumerate().map(move |(i, item)| {
        let dismiss = dismiss.clone();
        let run = item.run.clone();
        div()
            .flex()
            .flex_col()
            .when(item.separated, |el| {
                el.child(div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(theme::line()))
            })
            .child(
                div()
                    .id(ElementId::NamedInteger("menu-item".into(), i as u64))
                    .h(px(30.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::text_title())
                    .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                    .on_click(move |_, window, cx| {
                        dismiss(window, cx);
                        run(window, cx);
                    })
                    .child(icon(item.icon, px(16.), theme::text_muted()))
                    .child(div().flex_1().child(item.label))
                    .when_some(item.hint, |el, hint| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::text_meta())
                                .child(hint),
                        )
                    }),
            )
    });
    let panel = div()
        .id("context-menu")
        .occlude()
        .w(px(232.))
        .p(px(4.))
        .flex()
        .flex_col()
        .bg(theme::popover())
        .rounded(px(12.))
        .shadow_lg()
        .on_mouse_down_out(move |_, window, cx| dismiss_out(window, cx))
        .children(header)
        .children(rows)
        .with_animation(
            ElementId::NamedInteger("menu-open".into(), serial),
            anim::ease(anim::FAST),
            |el, t| el.opacity(t),
        );
    deferred(
        anchored()
            .position(position)
            .anchor(corner)
            .snap_to_window_with_margin(px(8.))
            .child(panel),
    )
    .with_priority(2)
}

// ---- Toast ------------------------------------------------------------------

/// The result of something the user started: a copy, a download, an upload.
#[derive(Clone)]
pub enum Status {
    Working(SharedString),
    Done {
        message: SharedString,
        /// Offer "Show in folder" for a saved file or folder.
        reveal: Option<PathBuf>,
    },
    Failed(SharedString),
    /// Done, with a moment to take it back.
    Undoable {
        message: SharedString,
        undo: Handler,
    },
    /// New items are in the gallery, with a way to go to them.
    Listed {
        message: SharedString,
        show: Handler,
    },
}

/// A message floating at the bottom of its (relative) parent.
pub fn toast(serial: u64, status: Status) -> impl IntoElement {
    let content = match status {
        Status::Working(text) => div()
            .text_color(theme::text_title())
            .child(loading_status("toast-working", text, theme::text_muted()))
            .into_any_element(),
        Status::Done { message, reveal } => div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(icon("check", px(15.), theme::accent_text()))
            .child(div().text_color(theme::text()).child(message))
            .when_some(reveal, |el, path| {
                el.child(link("toast-reveal", "Show in folder", move |_, _, cx| {
                    cx.reveal_path(&path)
                }))
            })
            .into_any_element(),
        Status::Failed(text) => div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(icon("warning", px(15.), theme::danger()))
            .child(div().text_color(theme::danger()).child(text))
            .into_any_element(),
        Status::Undoable { message, undo } => div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(icon("check", px(15.), theme::accent_text()))
            .child(div().text_color(theme::text()).child(message))
            .child(link("toast-undo", "Undo", move |_, window, cx| {
                undo(window, cx)
            }))
            .into_any_element(),
        Status::Listed { message, show } => div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(icon("check", px(15.), theme::accent_text()))
            .child(div().text_color(theme::text()).child(message))
            .child(link("toast-show", "Show", move |_, window, cx| {
                show(window, cx)
            }))
            .into_any_element(),
    };
    div()
        .absolute()
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .occlude()
                .h(px(40.))
                .max_w(px(520.))
                .px(px(16.))
                .flex()
                .items_center()
                .rounded(px(12.))
                .bg(theme::popover())
                .shadow_lg()
                .text_size(px(13.))
                .child(content),
        )
        .with_animation(
            ElementId::NamedInteger("toast".into(), serial),
            anim::ease(anim::NORMAL),
            |el, t| el.bottom(px(8. + 16. * t)).opacity(t),
        )
}

// ---- Dialog -----------------------------------------------------------------

/// A question over a dimmed view, with Cancel and one confirming button.
pub fn confirm_dialog(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    confirm: impl Into<SharedString>,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    dialog(title, body, confirm, false, on_confirm, on_cancel)
}

/// [`confirm_dialog`] for something destructive: the button is red.
pub fn destructive_dialog(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    confirm: impl Into<SharedString>,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    dialog(title, body, confirm, true, on_confirm, on_cancel)
}

fn dialog(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    confirm: impl Into<SharedString>,
    destructive: bool,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_cancel: Handler = Rc::new(on_cancel);
    let cancel_out = on_cancel.clone();
    div()
        .id("dialog-scrim")
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::scrim(0.6))
        .child(
            div()
                .id("dialog")
                .w(px(380.))
                .p(px(20.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .rounded(theme::RADIUS_DIALOG)
                .bg(theme::popover())
                .shadow_lg()
                .on_mouse_down_out(move |_, window, cx| cancel_out(window, cx))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text())
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child(body.into()),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .pt(px(12.))
                        .child(text_button(
                            "dialog-cancel",
                            None,
                            "Cancel",
                            move |_, window, cx| on_cancel(window, cx),
                        ))
                        .child(filled_button(
                            "dialog-confirm",
                            confirm,
                            destructive,
                            move |_, window, cx| on_confirm(window, cx),
                        )),
                ),
        )
        .with_animation("dialog-open", anim::ease(anim::FAST), |el, t| el.opacity(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("ada"), "A");
        assert_eq!(initials("Jean Luc Picard"), "JP");
        assert_eq!(initials(""), "");
    }
}
