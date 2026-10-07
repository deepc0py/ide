//! Rendering for the shared extension host's server -> client UI prompts
//! (`window/showInputBox`, `window/showQuickPick`, `window/showMessageRequest`).
//!
//! The proxy forwards each host request to core as a
//! [`CoreNotification`](lapce_rpc::core::CoreNotification) `IdeShow*` variant,
//! which [`WindowTabData::handle_core_notification`](crate::window_tab::WindowTabData)
//! stores in [`IdeExtData::active_prompt`](crate::ide_ext::IdeExtData). This view
//! renders the active prompt as a centered overlay (mirroring the command
//! palette / keymap picker). On submit/select the user's choice is sent back to
//! the proxy via [`ProxyRpcHandler::ide_prompt_response`](lapce_rpc::proxy::ProxyRpcHandler)
//! using the exact contract JSON; `Esc` (or dismissing the overlay) sends JSON
//! `null` to cancel.

use std::{rc::Rc, sync::Arc};

use floem::{
    IntoView, View,
    event::{Event, EventListener},
    keyboard::{Key, NamedKey},
    reactive::{
        RwSignal, SignalGet, SignalUpdate, SignalWith, create_effect,
        create_rw_signal,
    },
    style::{CursorStyle, Display},
    text::Weight,
    views::{
        Decorators, container, dyn_container, dyn_stack, empty, label, scroll,
        stack, svg, text,
    },
};
use lapce_rpc::ide_ext::QuickPickItem;
use serde_json::{Value, json};

use crate::{
    config::{LapceConfig, color::LapceColor, icon::LapceIcons},
    ide_ext::IdePrompt,
    window_tab::WindowTabData,
};

/// Send the user's reply for `id` to the proxy and dismiss the active prompt.
/// `result` is the exact reply JSON per the prompt's contract, or `Value::Null`
/// to cancel.
fn respond(window_tab_data: &Rc<WindowTabData>, id: u64, result: Value) {
    window_tab_data.common.proxy.ide_prompt_response(id, result);
    window_tab_data.ide_ext.clear_prompt();
}

/// Items of the active quick pick filtered by `filter` (case-insensitive match
/// against label and description), or empty when the active prompt isn't a
/// quick pick.
fn filtered_quick_pick(
    window_tab_data: &Rc<WindowTabData>,
    filter: RwSignal<String>,
) -> Vec<QuickPickItem> {
    match window_tab_data.ide_ext.active_prompt.get_untracked() {
        Some(IdePrompt::QuickPick { items, .. }) => {
            let needle = filter.get_untracked().to_lowercase();
            if needle.is_empty() {
                items
            } else {
                items
                    .into_iter()
                    .filter(|item| {
                        item.label.to_lowercase().contains(&needle)
                            || item
                                .description
                                .as_deref()
                                .map(|d| d.to_lowercase().contains(&needle))
                                .unwrap_or(false)
                    })
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

pub fn ide_prompt_view(window_tab_data: Rc<WindowTabData>) -> impl View {
    let config = window_tab_data.common.config;
    let ide_ext = window_tab_data.ide_ext.clone();
    let active_prompt = ide_ext.active_prompt;

    // Shared, reset-on-change edit state: text for the input box / quick-pick
    // filter and the highlighted quick-pick row.
    let filter = create_rw_signal(String::new());
    let selected = create_rw_signal(0usize);

    create_effect(move |_| {
        match active_prompt.get() {
            Some(IdePrompt::InputBox { value, .. }) => {
                filter.set(value.unwrap_or_default());
            }
            Some(IdePrompt::QuickPick { .. }) => {
                filter.set(String::new());
                selected.set(0);
            }
            _ => {}
        };
    });

    let dialog = dyn_container(
        move || active_prompt.get(),
        {
            let window_tab_data = window_tab_data.clone();
            move |prompt| match prompt {
                Some(IdePrompt::InputBox {
                    id,
                    title,
                    prompt,
                    place_holder,
                    value: _,
                    password,
                }) => input_box_view(
                    window_tab_data.clone(),
                    filter,
                    id,
                    title,
                    prompt,
                    place_holder,
                    password,
                )
                .into_any(),
                Some(IdePrompt::QuickPick {
                    id,
                    title,
                    place_holder,
                    items,
                }) => quick_pick_view(
                    window_tab_data.clone(),
                    filter,
                    selected,
                    id,
                    title,
                    place_holder,
                    items,
                )
                .into_any(),
                Some(IdePrompt::MessageRequest {
                    id,
                    typ,
                    message,
                    modal,
                    actions,
                }) => message_request_view(
                    window_tab_data.clone(),
                    id,
                    typ,
                    message,
                    modal,
                    actions,
                )
                .into_any(),
                None => empty().into_any(),
            }
        },
    )
    .style(move |s| {
        s.items_center().justify_center().margin_top(100.0).apply_if(
            active_prompt.with(|p| p.is_none()),
            |s| s.hide(),
        )
    });
    let view = container(dialog)
        .keyboard_navigable()
        .on_event_stop(EventListener::PointerDown, {
            // Clicking the dim backdrop cancels non-modal prompts.
            let window_tab_data = window_tab_data.clone();
            move |_| {
                if let Some(prompt) = active_prompt.get_untracked() {
                    let modal = matches!(
                        prompt,
                        IdePrompt::MessageRequest { modal: true, .. }
                    );
                    if !modal {
                        respond(&window_tab_data, prompt.id(), Value::Null);
                    }
                }
            }
        })
        .on_event_stop(EventListener::KeyDown, {
            let window_tab_data = window_tab_data.clone();
            move |event| {
                let Event::KeyDown(key_event) = event else {
                    return;
                };
                let Some(prompt) = active_prompt.get_untracked() else {
                    return;
                };
                match &key_event.key.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        respond(&window_tab_data, prompt.id(), Value::Null);
                    }
                    Key::Named(NamedKey::Enter) => match prompt {
                        IdePrompt::InputBox { id, .. } => {
                            respond(
                                &window_tab_data,
                                id,
                                json!({ "value": filter.get_untracked() }),
                            );
                        }
                        IdePrompt::QuickPick { id, .. } => {
                            let items =
                                filtered_quick_pick(&window_tab_data, filter);
                            if let Some(item) =
                                items.get(selected.get_untracked())
                            {
                                respond(
                                    &window_tab_data,
                                    id,
                                    json!({ "handle": item.handle }),
                                );
                            }
                        }
                        IdePrompt::MessageRequest { id, actions, .. } => {
                            if let Some(title) = actions.first() {
                                respond(
                                    &window_tab_data,
                                    id,
                                    json!({ "title": title }),
                                );
                            } else {
                                respond(&window_tab_data, id, Value::Null);
                            }
                        }
                    },
                    Key::Named(NamedKey::Backspace) => {
                        if matches!(
                            prompt,
                            IdePrompt::InputBox { .. }
                                | IdePrompt::QuickPick { .. }
                        ) {
                            filter.update(|v| {
                                v.pop();
                            });
                            if matches!(prompt, IdePrompt::QuickPick { .. }) {
                                selected.set(0);
                            }
                        }
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        if matches!(prompt, IdePrompt::QuickPick { .. }) {
                            let len =
                                filtered_quick_pick(&window_tab_data, filter)
                                    .len();
                            if len > 0 {
                                selected.update(|i| {
                                    *i = (*i + 1).min(len - 1);
                                });
                            }
                        }
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        if matches!(prompt, IdePrompt::QuickPick { .. }) {
                            selected.update(|i| {
                                *i = i.saturating_sub(1);
                            });
                        }
                    }
                    Key::Character(c) => {
                        if matches!(
                            prompt,
                            IdePrompt::InputBox { .. }
                                | IdePrompt::QuickPick { .. }
                        ) {
                            filter.update(|v| v.push_str(c));
                            if matches!(prompt, IdePrompt::QuickPick { .. }) {
                                selected.set(0);
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .style(move |s| {
            s.absolute()
                .size_pct(100.0, 100.0)
                .flex_col()
                .items_center()
                .justify_center()
                .apply_if(active_prompt.with(|p| p.is_none()), |s| {
                    s.display(Display::None)
                })
                .background(
                    config
                        .get()
                        .color(LapceColor::LAPCE_DROPDOWN_SHADOW)
                        .multiply_alpha(0.3),
                )
        })
        .debug_name("Ide Prompt Layer");

    let id = view.id();
    create_effect(move |_| {
        if active_prompt.with(|p| p.is_some()) {
            id.request_focus();
        }
    });

    view
}

fn dialog_container<V: View + 'static>(
    config: floem::reactive::ReadSignal<Arc<LapceConfig>>,
    child: V,
) -> impl View {
    container(child)
        .on_event_stop(EventListener::PointerDown, |_| {})
        .style(move |s| {
            let config = config.get();
            s.flex_col()
                .width(440.0)
                .max_width_pct(90.0)
                .padding(16.0)
                .border(1.0)
                .border_radius(6.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
                .background(config.color(LapceColor::PALETTE_BACKGROUND))
                .pointer_events_auto()
        })
}

fn edit_field(
    filter: RwSignal<String>,
    place_holder: Option<String>,
    password: bool,
    config: floem::reactive::ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let place_holder = place_holder.unwrap_or_default();
    container(
        label(move || {
            let value = filter.get();
            if value.is_empty() {
                place_holder.clone()
            } else if password {
                "\u{2022}".repeat(value.chars().count())
            } else {
                value
            }
        })
        .style(move |s| {
            let config = config.get();
            let empty = filter.with(|v| v.is_empty());
            s.color(if empty {
                config.color(LapceColor::EDITOR_DIM)
            } else {
                config.color(LapceColor::EDITOR_FOREGROUND)
            })
        }),
    )
    .style(move |s| {
        let config = config.get();
        s.width_pct(100.0)
            .padding_horiz(8.0)
            .padding_vert(6.0)
            .margin_top(8.0)
            .border(1.0)
            .border_radius(6.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::EDITOR_BACKGROUND))
    })
}

fn hint_text(
    msg: &'static str,
    config: floem::reactive::ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    text(msg).style(move |s| {
        s.margin_top(10.0)
            .font_size(11.0)
            .color(config.get().color(LapceColor::EDITOR_DIM))
    })
}

#[allow(clippy::too_many_arguments)]
fn input_box_view(
    window_tab_data: Rc<WindowTabData>,
    filter: RwSignal<String>,
    _id: u64,
    title: Option<String>,
    prompt: Option<String>,
    place_holder: Option<String>,
    password: bool,
) -> impl View {
    let config = window_tab_data.common.config;
    let title = title.unwrap_or_default();
    let prompt = prompt.unwrap_or_default();
    let show_prompt = !prompt.is_empty();
    let show_title = !title.is_empty();

    dialog_container(
        config,
        stack((
            text(title)
                .style(move |s| {
                    s.font_weight(Weight::BOLD)
                        .line_height(1.6)
                        .apply_if(!show_title, |s| s.hide())
                }),
            text(prompt).style(move |s| {
                s.margin_top(4.0)
                    .color(config.get().color(LapceColor::EDITOR_DIM))
                    .apply_if(!show_prompt, |s| s.hide())
            }),
            edit_field(filter, place_holder, password, config),
            hint_text("Enter to confirm · Esc to cancel", config),
        ))
        .style(|s| s.flex_col().width_pct(100.0)),
    )
}

#[allow(clippy::too_many_arguments)]
fn quick_pick_view(
    window_tab_data: Rc<WindowTabData>,
    filter: RwSignal<String>,
    selected: RwSignal<usize>,
    id: u64,
    title: Option<String>,
    place_holder: Option<String>,
    _items: Vec<QuickPickItem>,
) -> impl View {
    let config = window_tab_data.common.config;
    let title = title.unwrap_or_default();
    let show_title = !title.is_empty();

    let list = {
        let window_tab_data = window_tab_data.clone();
        let list_data = window_tab_data.clone();
        scroll(
            dyn_stack(
                move || {
                    filtered_quick_pick(&list_data, filter)
                        .into_iter()
                        .enumerate()
                        .collect::<Vec<_>>()
                },
                |(_, item): &(usize, QuickPickItem)| item.handle,
                move |(index, item)| {
                    let window_tab_data = window_tab_data.clone();
                    let handle = item.handle;
                    let label_text = item.label.clone();
                    let description = item.description.clone().unwrap_or_default();
                    let detail = item.detail.clone().unwrap_or_default();
                    let show_desc = !description.is_empty();
                    let show_detail = !detail.is_empty();
                    stack((
                        stack((
                            text(label_text).style(|s| s.line_height(1.6)),
                            text(description).style(move |s| {
                                s.margin_left(8.0)
                                    .color(
                                        config
                                            .get()
                                            .color(LapceColor::EDITOR_DIM),
                                    )
                                    .apply_if(!show_desc, |s| s.hide())
                            }),
                        ))
                        .style(|s| s.items_center()),
                        text(detail).style(move |s| {
                            s.font_size(11.0)
                                .color(config.get().color(LapceColor::EDITOR_DIM))
                                .apply_if(!show_detail, |s| s.hide())
                        }),
                    ))
                    .on_click_stop(move |_| {
                        respond(
                            &window_tab_data,
                            id,
                            json!({ "handle": handle }),
                        );
                    })
                    .style(move |s| {
                        let config = config.get();
                        s.flex_col()
                            .width_pct(100.0)
                            .padding_horiz(8.0)
                            .padding_vert(6.0)
                            .border_radius(6.0)
                            .apply_if(selected.get() == index, |s| {
                                s.background(
                                    config.color(
                                        LapceColor::PALETTE_CURRENT_BACKGROUND,
                                    ),
                                )
                            })
                            .hover(|s| {
                                s.cursor(CursorStyle::Pointer).background(
                                    config.color(
                                        LapceColor::PANEL_HOVERED_BACKGROUND,
                                    ),
                                )
                            })
                    })
                },
            )
            .style(|s| s.flex_col().width_pct(100.0)),
        )
        .style(|s| s.width_pct(100.0).max_height(300.0).margin_top(8.0))
    };

    dialog_container(
        config,
        stack((
            text(title).style(move |s| {
                s.font_weight(Weight::BOLD)
                    .line_height(1.6)
                    .apply_if(!show_title, |s| s.hide())
            }),
            edit_field(filter, place_holder, false, config),
            list,
            hint_text("↑↓ to navigate · Enter to select · Esc to cancel", config),
        ))
        .style(|s| s.flex_col().width_pct(100.0)),
    )
}

fn message_request_view(
    window_tab_data: Rc<WindowTabData>,
    id: u64,
    typ: u8,
    message: String,
    _modal: bool,
    actions: Vec<String>,
) -> impl View {
    let config = window_tab_data.common.config;

    let icon = svg(move || {
        let config = config.get();
        match typ {
            1 => config.ui_svg(LapceIcons::ERROR),
            _ => config.ui_svg(LapceIcons::WARNING),
        }
    })
    .style(move |s| {
        let config = config.get();
        let size = config.ui.icon_size() as f32;
        let color = match typ {
            1 => config.color(LapceColor::LAPCE_ERROR),
            2 => config.color(LapceColor::LAPCE_WARN),
            _ => config.color(LapceColor::EDITOR_FOREGROUND),
        };
        s.min_width(size)
            .size(size, size)
            .margin_right(10.0)
            .margin_top(2.0)
            .color(color)
    });

    let buttons = dyn_stack(
        move || actions.clone().into_iter().enumerate().collect::<Vec<_>>(),
        |(index, _): &(usize, String)| *index,
        move |(index, title)| {
            let window_tab_data = window_tab_data.clone();
            let reply = title.clone();
            text(title)
                .on_click_stop(move |_| {
                    respond(
                        &window_tab_data,
                        id,
                        json!({ "title": reply.clone() }),
                    );
                })
                .style(move |s| {
                    let config = config.get();
                    s.padding_horiz(14.0)
                        .padding_vert(6.0)
                        .border(1.0)
                        .border_radius(6.0)
                        .border_color(config.color(LapceColor::LAPCE_BORDER))
                        .apply_if(index > 0, |s| s.margin_left(8.0))
                        .hover(|s| {
                            s.cursor(CursorStyle::Pointer).background(
                                config.color(LapceColor::PANEL_HOVERED_BACKGROUND),
                            )
                        })
                        .active(|s| {
                            s.background(config.color(
                                LapceColor::PANEL_HOVERED_ACTIVE_BACKGROUND,
                            ))
                        })
                })
        },
    )
    .style(|s| {
        s.width_pct(100.0)
            .margin_top(16.0)
            .justify_end()
            .items_center()
    });

    dialog_container(
        config,
        stack((
            stack((
                icon,
                text(message).style(|s| s.line_height(1.6).min_width(0.0)),
            ))
            .style(|s| s.items_start().width_pct(100.0)),
            buttons,
        ))
        .style(|s| s.flex_col().width_pct(100.0)),
    )
}
