//! Drawing the gallery: header, mosaic, tiles, scrubber and menus.

use super::*;

impl PhotoGrid {
    // ---- Rendering --------------------------------------------------------------

    pub(super) fn ensure_layout(&mut self, width: f32) {
        let stale = self
            .layout
            .as_ref()
            .is_none_or(|l| (l.width - width).abs() > 0.5);
        if !(self.layout_dirty || stale) || width <= 0. {
            return;
        }
        let today = jiff::Zoned::now().date();
        let params = layout::Params {
            width,
            padding_x: theme::CONTENT_PADDING,
            padding_right: SCRUBBER_WIDTH - theme::CONTENT_PADDING,
            target_height: self.row_height.get(),
            gap: TILE_GAP,
            top_padding: HEADER_SPACE,
        };
        self.layout = Some(layout::build(&self.index, &self.days, params, today));
        self.layout_dirty = false;
    }

    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let sync_status: Option<SharedString> = match (&self.scan, &self.state) {
            (Some(scan), LoadState::Ready) if scan.progressive => Some(
                format!(
                    "Loading {} of {}\u{2026}",
                    group_digits(self.index.len()),
                    group_digits(scan.count)
                )
                .into(),
            ),
            (Some(_), _) => Some("Updating\u{2026}".into()),
            _ if self.syncing == Some(true) => Some("Checking for changes\u{2026}".into()),
            _ => None,
        };
        // A device's search in progress says so in words.
        let status = self
            .device_pending
            .as_ref()
            .map(|name| SharedString::from(format!("Asking {name}\u{2026}")));
        let syncing = sync_status.is_some();
        let sync = div()
            .id("sync")
            .size(theme::CONTROL_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .hover(|el| el.bg(theme::hover()))
            .tooltip(ui::tooltip(
                sync_status.unwrap_or_else(|| "Check for new photos".into()),
            ))
            .on_click(cx.listener(|this, _, _, cx| this.refresh(false, cx)))
            .child(if syncing {
                ui::spinner("sync-spinner", px(15.), theme::text_muted())
            } else {
                icon("sync", px(16.), theme::text_muted()).into_any_element()
            });
        // While a device searches and ours found nothing, no count yet.
        let waiting = self.device_pending.is_some() && self.index.is_empty();
        let count = (matches!(self.state, LoadState::Ready) && !waiting).then(|| {
            if let Some(search) = &self.search {
                let n = self.index.len();
                let count = format!(
                    "{} {}",
                    group_digits(n),
                    if n == 1 { "result" } else { "results" }
                );
                match &search.query {
                    Matcher::Device {
                        provider, capped, ..
                    } => {
                        let plus = if *capped { "+" } else { "" };
                        let count = format!(
                            "{}{plus} {}",
                            group_digits(n),
                            if n == 1 && plus.is_empty() {
                                "result"
                            } else {
                                "results"
                            }
                        );
                        format!("{count} \u{00b7} Provided by {provider}")
                    }
                    Matcher::Local(_) => count,
                }
            } else {
                format_count(self.scan.as_ref().map_or(self.index.len(), |s| s.count))
            }
        });
        let chosen = self.selected.len();

        let tools = if chosen > 0 {
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    div()
                        .mr(px(8.))
                        .text_sm()
                        .text_color(theme::accent_text())
                        .child(format!("{} selected", group_digits(chosen))),
                )
                // Plain icons, as in the viewer's bar.
                .child(
                    ui::icon_button(
                        "selection-copy",
                        "copy",
                        cx.listener(|this, _, window, cx| {
                            this.copy_selection(&CopySelection, window, cx)
                        }),
                    )
                    .tooltip(ui::tooltip("Copy (Ctrl+C)")),
                )
                .child(
                    ui::icon_button(
                        "selection-download",
                        "download",
                        cx.listener(|this, _, window, cx| {
                            this.download_selection(&DownloadSelection, window, cx)
                        }),
                    )
                    .tooltip(ui::tooltip("Download (Ctrl+S)")),
                )
                .when(self.can_delete(), |el| {
                    el.child(
                        ui::icon_button(
                            "selection-delete",
                            "deleted",
                            cx.listener(|this, _, window, cx| {
                                this.delete_selection(&DeleteSelection, window, cx)
                            }),
                        )
                        .tooltip(ui::tooltip("Delete (Del)")),
                    )
                })
                .child(
                    ui::icon_button(
                        "selection-clear",
                        "close",
                        cx.listener(|this, _, _, cx| this.clear_selection(cx)),
                    )
                    .tooltip(ui::tooltip("Clear selection (Esc)")),
                )
                .with_animation("selection-tools", anim::ease(anim::NORMAL), |el, t| {
                    el.opacity(t)
                })
                .into_any_element()
        } else {
            let weak = cx.weak_entity();
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(sync)
                .child(div().w(px(6.)))
                .child(
                    ui::icon_button(
                        "zoom-out",
                        "minus",
                        cx.listener(|this, _, window, cx| this.zoom_out(&ZoomOut, window, cx)),
                    )
                    .tooltip(ui::tooltip("Smaller (Ctrl+-)")),
                )
                .child(ui::slider(
                    "zoom-slider",
                    settings::zoom_fraction(self.row_height.target()),
                    px(110.),
                    move |fraction, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.zoom_to(settings::row_height_at(fraction), None, cx)
                        })
                        .ok();
                    },
                ))
                .child(
                    ui::icon_button(
                        "zoom-in",
                        "plus",
                        cx.listener(|this, _, window, cx| this.zoom_in(&ZoomIn, window, cx)),
                    )
                    .tooltip(ui::tooltip("Larger (Ctrl+=)")),
                )
                .into_any_element()
        };

        // Under the title: the month in view, then the count.
        let month = self
            .month_in_view
            .filter(|_| self.search.is_none())
            .map(|(year, month)| format!("{} {year}", layout::month_name(month)));
        let subtitle: Vec<String> = month.into_iter().chain(count).collect();
        let mut translucent = theme::surface();
        translucent.a = 0.86;

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            // Scrolling goes through to the mosaic; clicks don't.
            .block_mouse_except_scroll()
            .flex()
            .items_center()
            .gap_3()
            .px(px(theme::CONTENT_PADDING))
            .pt(px(10.))
            .pb(px(12.))
            // Flat and translucent: the photos pass faintly under it.
            .bg(translucent)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(self.back, |el| {
                                el.ml(px(-8.)).child(
                                    ui::icon_button(
                                        "gallery-back",
                                        "previous",
                                        cx.listener(|_, _, _, cx| cx.emit(GridEvent::Back)),
                                    )
                                    .tooltip(ui::tooltip("Collections")),
                                )
                            })
                            .child(
                                div()
                                    .text_size(px(28.))
                                    .line_height(px(32.))
                                    .font_weight(FontWeight::BLACK)
                                    .text_color(theme::text())
                                    .child(self.title.clone()),
                            ),
                    )
                    .child(
                        div()
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(13.))
                            .children(subtitle.into_iter().enumerate().map(|(i, part)| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .when(i > 0, |el| {
                                        el.child(
                                            div().text_color(theme::text_meta()).child("\u{00b7}"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .when(i == 0, |el| {
                                                el.font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme::text_title())
                                            })
                                            .when(i > 0, |el| el.text_color(theme::text_meta()))
                                            .child(part),
                                    )
                            })),
                    ),
            )
            .child(div().flex_1())
            .when_some(status, |el, status| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_meta())
                        .child(ui::loading_status(
                            "header-status",
                            status,
                            theme::text_meta(),
                        )),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .child(icon("warning", px(14.), theme::danger()))
                        .child(div().text_color(theme::danger()).child(error))
                        .child(retry_link(cx)),
                )
            })
            .when(matches!(self.state, LoadState::Ready), |el| el.child(tools))
    }

    pub(super) fn render_menu(
        &self,
        menu: &OpenMenu,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let this = cx.weak_entity();
        // Each entry runs against the grid, once the menu has closed.
        let run = |f: fn(&mut Self, &[usize], &mut Window, &mut Context<Self>),
                   targets: Vec<usize>| {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| f(this, &targets, window, cx))
                    .ok();
            }
        };
        let mut items = Vec::new();
        match &menu.kind {
            MenuKind::Items(targets) => {
                let many = targets.len() > 1;
                if !many {
                    items.push(MenuItem::new(
                        "open",
                        "Open",
                        run(
                            |this, t, window, cx| this.open_viewer(t[0], window, cx),
                            targets.clone(),
                        ),
                    ));
                }
                items.push(
                    MenuItem::new(
                        "copy",
                        if many {
                            format!("Copy {} items", targets.len())
                        } else {
                            "Copy".into()
                        },
                        run(|this, t, _, cx| this.copy(t.to_vec(), cx), targets.clone()),
                    )
                    .hint("Ctrl+C"),
                );
                items.push(
                    MenuItem::new(
                        "download",
                        if many {
                            format!("Download {} originals\u{2026}", targets.len())
                        } else {
                            "Download original\u{2026}".into()
                        },
                        run(
                            |this, t, _, cx| this.download(t.to_vec(), cx),
                            targets.clone(),
                        ),
                    )
                    .hint("Ctrl+S"),
                );
                let all_selected = targets.iter().all(|ix| self.selected.contains(ix));
                items.push(
                    MenuItem::new(
                        "select",
                        if all_selected { "Deselect" } else { "Select" },
                        run(
                            |this, t, _, cx| {
                                if t.iter().all(|ix| this.selected.contains(ix)) {
                                    for ix in t {
                                        this.selected.remove(ix);
                                    }
                                } else {
                                    this.selected.extend(t.iter().copied());
                                    this.select_anchor = t.first().copied();
                                }
                                cx.notify();
                            },
                            targets.clone(),
                        ),
                    )
                    .separated(),
                );
                if !self.selected.is_empty() {
                    items.push(MenuItem::new(
                        "close",
                        "Clear selection",
                        run(|this, _, _, cx| this.clear_selection(cx), Vec::new()),
                    ));
                }
                if self.can_favorite() {
                    let verb = if self.all_favorite(targets) {
                        "Unfavorite"
                    } else {
                        "Favorite"
                    };
                    items.push(
                        MenuItem::new(
                            "favorites",
                            if many {
                                format!("{verb} {} items", targets.len())
                            } else {
                                verb.into()
                            },
                            run(
                                |this, t, _, cx| this.toggle_favorite(t.to_vec(), cx),
                                targets.clone(),
                            ),
                        )
                        .hint(".")
                        .separated(),
                    );
                }
                if self.can_delete() {
                    items.push(
                        MenuItem::new(
                            "deleted",
                            if many {
                                format!("Delete {} items\u{2026}", targets.len())
                            } else {
                                "Delete\u{2026}".into()
                            },
                            run(
                                |this, t, _, cx| this.ask_delete(t.to_vec(), cx),
                                targets.clone(),
                            ),
                        )
                        .hint("Del")
                        .separated(),
                    );
                }
            }
            MenuKind::Background => {
                items.push(MenuItem::new(
                    "upload",
                    "Upload photos and videos\u{2026}",
                    run(
                        |this, _, window, cx| this.pick_uploads(window, cx),
                        Vec::new(),
                    ),
                ));
                items.push(
                    MenuItem::new(
                        "copy",
                        "Paste to add to iCloud",
                        run(|this, _, _, cx| this.paste(cx), Vec::new()),
                    )
                    .hint("Ctrl+V"),
                );
                items.push(
                    MenuItem::new(
                        "select",
                        "Select all",
                        run(
                            |this, _, window, cx| this.select_all(&SelectAll, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+A"),
                );
                items.push(
                    MenuItem::new(
                        "plus",
                        "Zoom in",
                        run(
                            |this, _, window, cx| this.zoom_in(&ZoomIn, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+=")
                    .separated(),
                );
                items.push(
                    MenuItem::new(
                        "minus",
                        "Zoom out",
                        run(
                            |this, _, window, cx| this.zoom_out(&ZoomOut, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+-"),
                );
            }
            MenuKind::Header { first, month } => {
                let first = *first;
                let year = self
                    .days
                    .get(first)
                    .copied()
                    .flatten()
                    .map(|day| day.year());
                let mut spans = vec![(Span::Month, "month".to_string())];
                if !*month {
                    spans.insert(0, (Span::Day, "day".to_string()));
                }
                if let Some(year) = year {
                    spans.push((Span::Year, format!("all of {year}")));
                }
                for (span, what) in spans {
                    let verb = if self.span_selected(first, span) {
                        "Deselect"
                    } else {
                        "Select"
                    };
                    let label = match span {
                        Span::Year => format!("{verb} {what}"),
                        _ => format!("{verb} this {what}"),
                    };
                    let this = this.clone();
                    items.push(MenuItem::new("select", label, move |_, cx| {
                        this.update(cx, |this, cx| this.toggle_span(first, span, cx))
                            .ok();
                    }));
                }
                if !self.selected.is_empty() {
                    items.push(
                        MenuItem::new(
                            "close",
                            "Clear selection",
                            run(|this, _, _, cx| this.clear_selection(cx), Vec::new()),
                        )
                        .separated(),
                    );
                }
            }
            MenuKind::Viewer(index) => {
                let target = vec![*index];
                items.push(
                    MenuItem::new(
                        "copy",
                        "Copy",
                        run(|this, t, _, cx| this.copy(t.to_vec(), cx), target.clone()),
                    )
                    .hint("Ctrl+C"),
                );
                items.push(
                    MenuItem::new(
                        "download",
                        "Download original\u{2026}",
                        run(
                            |this, t, _, cx| this.download(t.to_vec(), cx),
                            target.clone(),
                        ),
                    )
                    .hint("Ctrl+S"),
                );
                if self.can_favorite() {
                    let verb = if self.all_favorite(&target) {
                        "Unfavorite"
                    } else {
                        "Favorite"
                    };
                    items.push(
                        MenuItem::new(
                            "favorites",
                            verb,
                            run(
                                |this, t, _, cx| this.toggle_favorite(t.to_vec(), cx),
                                target.clone(),
                            ),
                        )
                        .hint(".")
                        .separated(),
                    );
                }
                if self.can_delete() {
                    items.push(
                        MenuItem::new(
                            "deleted",
                            "Delete\u{2026}",
                            run(|this, t, _, cx| this.ask_delete(t.to_vec(), cx), target),
                        )
                        .hint("Del"),
                    );
                }
            }
        }
        let this = cx.weak_entity();
        ui::context_menu(menu.serial, menu.position, items, move |_, cx| {
            this.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            })
            .ok();
        })
    }

    pub(super) fn render_upload_dialog(
        &self,
        upload: &PendingUpload,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let count = upload.items.len();
        let title = if count == 1 {
            "Add to iCloud Photos?".to_string()
        } else {
            format!("Add {count} items to iCloud Photos?")
        };
        let mut names: Vec<&str> = upload
            .items
            .iter()
            .take(3)
            .map(|i| i.name.as_str())
            .collect();
        if count > 3 {
            names.push("\u{2026}");
        }
        let body = format!(
            "{} will be uploaded to your library and appear on all your devices.",
            names.join(", ")
        );
        let confirm = cx.weak_entity();
        let cancel = cx.weak_entity();
        ui::confirm_dialog(
            title,
            body,
            "Add",
            move |_, cx| {
                confirm.update(cx, |this, cx| this.confirm_upload(cx)).ok();
            },
            move |_, cx| {
                cancel
                    .update(cx, |this, cx| {
                        this.pending_upload = None;
                        cx.notify();
                    })
                    .ok();
            },
        )
    }
    pub(super) fn render_mosaic(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The scroll area's size from the last frame; before the first
        // frame, estimate from the window.
        let bounds = self.scroll.bounds();
        let width = if bounds.size.width > px(0.) {
            f32::from(bounds.size.width)
        } else {
            f32::from(window.viewport_size().width)
        };
        let viewport_h = if bounds.size.height > px(0.) {
            f32::from(bounds.size.height)
        } else {
            f32::from(window.viewport_size().height)
        };

        // Zoom: re-lay out at the eased row height, holding the anchor still.
        let zooming = self.row_height.tick(ZOOM_RATE, 0.25);
        if zooming {
            self.layout_dirty = true;
            window.request_animation_frame();
        }
        // A new width re-flows the rows: keep the photo at the top in place.
        if self.zoom_anchor.is_none()
            && self.top() > 0.
            && self
                .layout
                .as_ref()
                .is_some_and(|l| (l.width - width).abs() > 0.5)
        {
            self.zoom_anchor = self.anchor_at(HEADER_SPACE);
        }
        self.ensure_layout(width);
        let Some(layout) = self.layout.take() else {
            return div().flex_1().into_any_element();
        };
        self.max_scroll = (layout.height - viewport_h).max(0.);
        if let Some(anchor) = &self.zoom_anchor {
            if let Some(row) = layout.row_of_item(anchor.index) {
                let top = row.y + anchor.fraction * row.height - anchor.screen_y;
                self.jump_to(top.clamp(0., self.max_scroll));
            }
            if !zooming {
                self.zoom_anchor = None;
            }
        }
        if !zooming && (Settings::get(cx).row_height - self.row_height.get()).abs() > 0.01 {
            // The zoom settled: remember it.
            let height = self.row_height.get();
            cx.defer(move |cx| Settings::update(cx, |settings| settings.row_height = height));
        }

        // Scroll: ease toward where the wheel or the scrubber pointed.
        if self.scroller.is_moving() {
            self.scroller
                .set(self.scroller.target().min(self.max_scroll));
            self.scroller.tick(SCROLL_RATE, 0.5);
            let top = self.scroller.get();
            self.scroll.set_offset(point(px(0.), px(-top)));
            window.request_animation_frame();
        }
        let top = self.top().clamp(0., self.max_scroll);
        let flying = self.scroller.is_moving()
            && !zooming
            && (self.scroller.target() - top).abs() > viewport_h * FLYING_VIEWPORTS;

        // The month under the header, once its own heading has gone under.
        self.month_in_view = layout
            .month_at(top + HEADER_SPACE * 0.7)
            .filter(|mark| mark.y + layout::MONTH_HEIGHT < top + HEADER_SPACE)
            .map(|mark| (mark.year, mark.month));

        let rows = layout.rows_in(top - OVERSCAN, top + viewport_h + OVERSCAN);
        if !flying {
            // Thumbnails for what's on screen, top to bottom.
            let on_screen = layout.rows_in(top, top + viewport_h);
            let visible: Vec<usize> = layout.rows[on_screen]
                .iter()
                .filter_map(|row| match &row.kind {
                    RowKind::Tiles(tiles) => Some(tiles.iter().map(|t| t.index)),
                    _ => None,
                })
                .flatten()
                .collect();
            self.request_visible(&visible, cx);
        }

        let selecting = !self.selected.is_empty();
        let menu_targets: &[usize] = match &self.menu {
            Some(OpenMenu {
                kind: MenuKind::Items(targets),
                ..
            }) => targets,
            _ => &[],
        };
        let mut children: Vec<AnyElement> = Vec::with_capacity(rows.len() * 6);
        let mut tiles_built = 0;
        for row in &layout.rows[rows] {
            match &row.kind {
                RowKind::Month(label) => children.push(
                    self.render_header_row(row, label, true, selecting, cx)
                        .items_end()
                        .pb(px(6.))
                        .text_size(px(20.))
                        .font_weight(FontWeight::BLACK)
                        .text_color(theme::text())
                        .into_any_element(),
                ),
                RowKind::Day(label) => children.push(
                    self.render_header_row(row, label, false, selecting, cx)
                        .items_center()
                        .text_size(px(13.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text_title())
                        .into_any_element(),
                ),
                RowKind::Tiles(tiles) => {
                    for t in tiles {
                        let Some(item) = self.index.get(t.index) else {
                            continue;
                        };
                        let thumb = (!flying)
                            .then(|| self.thumbs_ready.get(&item.asset_id()).cloned())
                            .flatten();
                        let look = TileLook {
                            selected: self.selected.contains(&t.index),
                            selecting,
                            highlighted: menu_targets.contains(&t.index),
                        };
                        tiles_built += 1;
                        children.push(
                            self.render_tile(t.index, item, thumb, look, cx)
                                .left(px(t.x))
                                .top(px(row.y))
                                .w(px(t.width))
                                .h(px(row.height))
                                .into_any_element(),
                        );
                    }
                }
            }
        }

        // Zoomed out, more tiles are on screen than the cache usually holds.
        let wanted = (tiles_built * 3 / 2).max(IMAGE_CACHE_SIZE);
        if wanted != self.image_cache_size && !zooming {
            self.image_cache_size = wanted;
            self.image_cache
                .update(cx, |cache, _| cache.set_capacity(wanted));
        }
        // Decode thumbnails no bigger than the tiles show them (once the
        // zoom settles, so it doesn't redo them on the way).
        if !zooming {
            let height =
                crate::image_cache::height_for(self.row_height.target(), window.scale_factor());
            self.image_cache
                .update(cx, |cache, _| cache.set_max_height(height));
        }

        let marks: Vec<f32> = layout.months.iter().map(|m| m.y).collect();
        let track = Track::new(
            layout.height,
            viewport_h,
            (viewport_h - SCRUBBER_TOP - SCRUBBER_INSET).max(1.),
            &marks,
        );
        self.track = Some(track.clone());
        let scale = window.scale_factor();
        let scrubber =
            (self.max_scroll > 0.).then(|| self.render_scrubber(&layout, track, top, scale, cx));
        let height = layout.height;
        self.layout = Some(layout);

        div()
            .image_cache(self.image_cache.clone())
            .id("gallery-area")
            .flex_1()
            .min_h_0()
            .relative()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_press(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_press(cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| this.focus_handle.focus(window)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    this.open_menu(MenuKind::Background, event.position, cx);
                }),
            )
            .child(
                div()
                    .id("gallery")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(height))
                            .on_scroll_wheel(cx.listener(Self::on_wheel))
                            .children(children),
                    ),
            )
            .children(scrubber)
            .into_any_element()
    }

    /// A month or day header. Its mark (on hover, or while selecting)
    /// selects everything under it; right-click offers the year too.
    pub(super) fn render_header_row(
        &self,
        row: &layout::Row,
        label: &str,
        month: bool,
        selecting: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let first = row.first;
        let span = if month { Span::Month } else { Span::Day };
        let chosen = selecting && self.span_selected(first, span);
        let group = SharedString::from(format!("header-{first}-{month}"));
        let mark = div()
            .id("mark")
            .size(px(18.))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_span(first, span, cx);
                cx.stop_propagation();
            }))
            .map(|el| {
                if chosen {
                    el.bg(theme::accent())
                        .child(icon("check", px(11.), gpui::white()))
                } else {
                    el.border_2()
                        .border_color(theme::text_meta())
                        .opacity(if selecting { 0.8 } else { 0. })
                        .group_hover(group.clone(), |el| el.opacity(1.))
                }
            });
        div()
            .id(ElementId::NamedInteger(
                if month { "month-header" } else { "day-header" }.into(),
                first as u64,
            ))
            .group(group)
            .absolute()
            .left(px(theme::CONTENT_PADDING))
            .right(px(SCRUBBER_WIDTH))
            .top(px(row.y))
            .h(px(row.height))
            .flex()
            .gap(px(10.))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    this.open_menu(MenuKind::Header { first, month }, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(label.to_string())
            .child(
                // Centre the mark on the label's line.
                div()
                    .h(px(if month { 26. } else { 18. }))
                    .flex()
                    .items_center()
                    .child(mark),
            )
    }

    pub(super) fn render_tile(
        &self,
        index: usize,
        item: &IndexItem,
        thumb: Option<Arc<Path>>,
        look: TileLook,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let group = SharedString::from(format!("tile-{index}"));
        let duration = match item.kind {
            Kind::Video(secs) => Some(format!("{}:{:02}", secs / 60, secs % 60)),
            _ => None,
        };
        let picture = div()
            .absolute()
            .overflow_hidden()
            .rounded(theme::RADIUS_TILE)
            .bg(theme::placeholder())
            .when_some(thumb, |el, path| {
                el.child(
                    img(path)
                        .size_full()
                        .object_fit(ObjectFit::Cover)
                        // Fade in when the thumbnail arrives.
                        .with_animation(
                            ElementId::NamedInteger("thumb-fade".into(), index as u64),
                            anim::ease(180),
                            |el, t| el.opacity(t),
                        ),
                )
            })
            // Hover: one flat scrim step, no lift or glow.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .when(look.highlighted, |el| el.bg(gpui::white().opacity(0.12)))
                    .group_hover(group.clone(), |el| el.bg(gpui::white().opacity(0.07))),
            )
            .when_some(duration, |el, duration| {
                el.child(
                    badge()
                        .right(px(6.))
                        .child(icon("play-fill", px(10.), gpui::white()))
                        .child(duration),
                )
            })
            .when(item.kind == Kind::Live, |el| {
                el.child(
                    badge()
                        .right(px(6.))
                        .child(icon("live", px(12.), gpui::white())),
                )
            })
            .when(item.favorite, |el| {
                el.child(
                    badge()
                        .left(px(6.))
                        .child(icon("heart-fill", px(11.), gpui::white())),
                )
            });
        // A selected photo steps back inside an accent frame.
        let picture = if look.selected {
            picture
                .with_animation(
                    ElementId::NamedInteger("tile-select".into(), index as u64),
                    anim::ease(anim::FAST),
                    |el, t| el.inset(px(6. * t)),
                )
                .into_any_element()
        } else {
            picture.inset_0().into_any_element()
        };

        // The round mark in the corner: shows on hover, selects on click.
        let mark = div()
            .id(ElementId::NamedInteger("tile-mark".into(), index as u64))
            .absolute()
            .top(px(6.))
            .left(px(6.))
            .size(px(22.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_selected(index, cx);
                cx.stop_propagation();
            }))
            .map(|el| {
                if look.selected {
                    el.bg(theme::accent())
                        .child(icon("check", px(13.), gpui::white()))
                } else {
                    el.bg(theme::scrim(0.35))
                        .border_2()
                        .border_color(gpui::white().opacity(0.9))
                        .opacity(if look.selecting { 0.75 } else { 0. })
                        .group_hover(group.clone(), |el| el.opacity(1.))
                }
            });

        div()
            .id(ElementId::NamedInteger("tile".into(), index as u64))
            .group(group)
            .absolute()
            .rounded(theme::RADIUS_TILE)
            .cursor_pointer()
            .when(look.selected, |el| el.bg(theme::accent_soft()))
            .child(picture)
            .child(mark)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    this.press = Some((index, event.position));
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    let targets = this.targets_for(index);
                    this.open_menu(MenuKind::Items(targets), event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(cx.listener(move |this, event, window, cx| {
                this.tile_clicked(index, event, window, cx)
            }))
    }

    pub(super) fn render_scrubber(
        &mut self,
        layout: &Layout,
        track: Track,
        top: f32,
        scale: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let thumb = track.position(top);
        // Hairlines on whole device pixels, so none blur away to nothing.
        let snap = move |y: f32| (y * scale).round() / scale;
        let hairline = scale.round().max(1.) / scale;
        let active = self.scrub.dragging || self.scrub.hover.is_some();
        // The date follows the pointer when it's over the track, and the
        // scroll position otherwise.
        let pointer = self.scrub.hover.filter(|_| !self.scrub.dragging);
        let date_at = pointer.unwrap_or(thumb);
        let date = (active || self.scrub.scrolled.is_some())
            .then(|| layout.month_at(track.scroll_for(date_at) + 1.))
            .flatten()
            .map(|mark| format!("{} {}", layout::month_name(mark.month), mark.year));

        let years = scrubber::year_labels(&layout.months, &track, 20.);
        let ticks = scrubber::month_ticks(&layout.months, &track, 6.);
        let ink = if active {
            theme::text_muted()
        } else {
            theme::text_meta()
        };

        // While dragging, follow the pointer anywhere in the window: over
        // the header or the title bar it pins to the top.
        let dragging = self.scrub.dragging;
        let weak = cx.weak_entity();
        let follow = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                if !dragging {
                    return;
                }
                let on_move = weak.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase.capture() {
                        return;
                    }
                    let held = event.pressed_button == Some(MouseButton::Left);
                    on_move
                        .update(cx, |this, cx| {
                            if !this.scrub.dragging {
                                return;
                            }
                            if held {
                                this.scrub_to(event.position, cx);
                            } else {
                                this.end_press(cx);
                            }
                        })
                        .ok();
                });
                let on_up = weak.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase.bubble() {
                        on_up.update(cx, |this, cx| this.end_press(cx)).ok();
                    }
                });
            },
        )
        .absolute()
        .size_0();

        div()
            .id("scrubber")
            .child(follow)
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(SCRUBBER_WIDTH))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.scrub.dragging = true;
                    this.scrub_to(event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(length) = this.track.as_ref().map(|t| t.length) {
                    let y = f32::from(event.position.y - this.scroll.bounds().top()) - SCRUBBER_TOP;
                    this.scrub.hover = Some(y.clamp(0., length));
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && !this.scrub.dragging {
                    this.scrub.hover = None;
                    cx.notify();
                }
            }))
            .children(ticks.into_iter().map(move |y| {
                div()
                    .absolute()
                    .right(px(10.))
                    .top(px(snap(SCRUBBER_TOP + y)))
                    .w(px(4.))
                    .h(px(hairline))
                    .bg(theme::line())
            }))
            .children(years.into_iter().map(|(y, year)| {
                div()
                    .absolute()
                    .right(px(18.))
                    .top(px(SCRUBBER_TOP + y - 7.))
                    .h(px(14.))
                    .flex()
                    .items_center()
                    .text_size(px(10.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ink)
                    .child(year.to_string())
            }))
            .when_some(pointer, |el, y| {
                el.child(
                    div()
                        .absolute()
                        .left(px(6.))
                        .right(px(6.))
                        .top(px(SCRUBBER_TOP + y))
                        .h(px(1.))
                        .bg(theme::text_meta()),
                )
            })
            // Where the gallery is scrolled to.
            .child(
                div()
                    .absolute()
                    .right(px(4.))
                    .top(px(SCRUBBER_TOP + thumb - 10.))
                    .w(px(3.))
                    .h(px(20.))
                    .rounded_full()
                    .bg(if active {
                        theme::accent_text()
                    } else {
                        theme::accent()
                    }),
            )
            .when_some(date, |el, date| {
                el.child(
                    div()
                        .absolute()
                        .right(px(SCRUBBER_WIDTH + 4.))
                        .top(px(SCRUBBER_TOP + date_at - 14.))
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(8.))
                        .bg(theme::popover())
                        .shadow_lg()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .whitespace_nowrap()
                        .child(date)
                        .with_animation("scrubber-date", anim::ease(anim::FAST), |el, t| {
                            el.opacity(t)
                        }),
                )
            })
    }
}
