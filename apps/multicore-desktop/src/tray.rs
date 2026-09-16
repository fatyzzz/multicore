#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    ShowWindow,
    ToggleConnection,
    SelectGroup { group_id: String },
    SelectNode { group_id: String, node_id: String },
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayGroup {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayInput {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) groups: Vec<TrayGroup>,
    pub(crate) selected_group_id: Option<String>,
    pub(crate) nodes: Vec<(String, String, bool)>,
    pub(crate) route_selection_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuEntry {
    pub(crate) label: String,
    pub(crate) checked: bool,
    pub(crate) enabled: bool,
    pub(crate) command: TrayCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuModel {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) groups: Vec<TrayMenuEntry>,
    pub(crate) nodes: Vec<TrayMenuEntry>,
}

pub(crate) fn build_menu_model(input: TrayInput) -> TrayMenuModel {
    let groups = input
        .groups
        .into_iter()
        .map(|group| TrayMenuEntry {
            label: group.label,
            checked: group.selected,
            enabled: input.route_selection_enabled,
            command: TrayCommand::SelectGroup { group_id: group.id },
        })
        .collect();
    let nodes = input
        .selected_group_id
        .map(|group_id| {
            input
                .nodes
                .into_iter()
                .map(|(node_id, label, selected)| TrayMenuEntry {
                    label,
                    checked: selected,
                    enabled: input.route_selection_enabled,
                    command: TrayCommand::SelectNode {
                        group_id: group_id.clone(),
                        node_id,
                    },
                })
                .collect()
        })
        .unwrap_or_default();

    TrayMenuModel {
        status: input.status,
        connection_label: input.connection_label,
        connection_enabled: input.connection_enabled,
        groups,
        nodes,
    }
}

#[cfg(windows)]
mod native {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::Duration;

    use slint::{Timer, TimerMode};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{TrayCommand, TrayMenuEntry, TrayMenuModel};

    const TRAY_GUID: u128 = 0x8ca0_87f4_4514_4ce5_91f1_7c6c_e181_46d1;

    pub(crate) struct TrayRuntime {
        _tray: Rc<RefCell<NativeTray>>,
        _timer: Timer,
    }

    struct NativeTray {
        icon: TrayIcon,
        menu: Menu,
        commands: HashMap<String, TrayCommand>,
        model: TrayMenuModel,
    }

    impl NativeTray {
        fn new(model: TrayMenuModel) -> Result<Self, String> {
            let (menu, commands) = native_menu(&model)?;
            let icon = Icon::from_rgba(crate::app_icon::rgba(32), 32, 32)
                .map_err(|error| format!("invalid tray icon: {error}"))?;
            let tray = TrayIconBuilder::new()
                .with_id("multicore-main")
                .with_guid(TRAY_GUID)
                .with_tooltip(tooltip(&model))
                .with_icon(icon)
                .with_menu(Box::new(menu.clone()))
                .with_menu_on_left_click(true)
                .build()
                .map_err(|error| format!("failed to create tray icon: {error}"))?;
            Ok(Self {
                icon: tray,
                menu,
                commands,
                model,
            })
        }

        fn sync(&mut self, model: TrayMenuModel) {
            if self.model == model {
                return;
            }
            match native_menu(&model) {
                Ok((menu, commands)) => {
                    self.icon.set_menu(Some(Box::new(menu.clone())));
                    let _ = self.icon.set_tooltip(Some(tooltip(&model)));
                    self.menu = menu;
                    self.commands = commands;
                    self.model = model;
                }
                Err(error) => eprintln!("tray menu refresh failed: {error}"),
            }
        }

        fn drain_events(&self) -> Vec<TrayCommand> {
            let mut events = Vec::new();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(command) = self.commands.get(&event.id.0) {
                    events.push(command.clone());
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    events.push(TrayCommand::ShowWindow);
                }
            }
            events
        }
    }

    pub(crate) fn start<Model, Action>(
        initial: TrayMenuModel,
        model_provider: Model,
        action_handler: Action,
    ) -> Result<TrayRuntime, String>
    where
        Model: Fn() -> TrayMenuModel + 'static,
        Action: Fn(TrayCommand) + 'static,
    {
        let tray = Rc::new(RefCell::new(NativeTray::new(initial)?));
        let timer = Timer::default();
        let timer_tray = tray.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(180), move || {
            let model = model_provider();
            let events = {
                let mut tray = timer_tray.borrow_mut();
                tray.sync(model);
                tray.drain_events()
            };
            for event in events {
                action_handler(event);
            }
        });
        Ok(TrayRuntime {
            _tray: tray,
            _timer: timer,
        })
    }

    fn native_menu(model: &TrayMenuModel) -> Result<(Menu, HashMap<String, TrayCommand>), String> {
        let menu = Menu::new();
        let mut commands = HashMap::new();

        let status = MenuItem::with_id(
            "status",
            format!("Статус: {}", menu_text(&model.status)),
            false,
            None,
        );
        let connection = MenuItem::with_id(
            "connection",
            menu_text(&model.connection_label),
            model.connection_enabled,
            None,
        );
        commands.insert("connection".into(), TrayCommand::ToggleConnection);

        let groups = Submenu::with_id("groups", "Группа", !model.groups.is_empty());
        append_entries(&groups, &model.groups, &mut commands)?;
        let nodes = Submenu::with_id("nodes", "Узел", !model.nodes.is_empty());
        append_entries(&nodes, &model.nodes, &mut commands)?;

        let open = MenuItem::with_id("open", "Открыть MultiCore", true, None);
        commands.insert("open".into(), TrayCommand::ShowWindow);
        let exit = MenuItem::with_id("exit", "Выйти", true, None);
        commands.insert("exit".into(), TrayCommand::Exit);

        menu.append_items(&[
            &status,
            &connection,
            &PredefinedMenuItem::separator(),
            &groups,
            &nodes,
            &PredefinedMenuItem::separator(),
            &open,
            &exit,
        ])
        .map_err(|error| format!("failed to populate tray menu: {error}"))?;
        Ok((menu, commands))
    }

    fn append_entries(
        submenu: &Submenu,
        entries: &[TrayMenuEntry],
        commands: &mut HashMap<String, TrayCommand>,
    ) -> Result<(), String> {
        for entry in entries {
            let id = route_menu_id(&entry.command)?;
            let item = CheckMenuItem::with_id(
                id.clone(),
                menu_text(&entry.label),
                entry.enabled,
                entry.checked,
                None,
            );
            submenu
                .append(&item)
                .map_err(|error| format!("failed to add tray route: {error}"))?;
            commands.insert(id, entry.command.clone());
        }
        Ok(())
    }

    fn route_menu_id(command: &TrayCommand) -> Result<String, String> {
        match command {
            TrayCommand::SelectGroup { group_id } => {
                Ok(format!("group:{}", encode_menu_id_component(group_id)))
            }
            TrayCommand::SelectNode { group_id, node_id } => Ok(format!(
                "node:{}:{}",
                encode_menu_id_component(group_id),
                encode_menu_id_component(node_id)
            )),
            _ => Err("non-route command cannot be added as a tray route".into()),
        }
    }

    fn encode_menu_id_component(value: &str) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(value.len() * 2);
        for byte in value.bytes() {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
        encoded
    }

    fn tooltip(model: &TrayMenuModel) -> String {
        let value = format!("MultiCore — {}", model.status);
        value.chars().take(120).collect()
    }

    fn menu_text(value: &str) -> String {
        value
            .chars()
            .take(96)
            .collect::<String>()
            .replace('&', "&&")
    }

    #[cfg(test)]
    mod tests {
        use super::native_menu;
        use crate::tray::{TrayCommand, TrayMenuEntry, TrayMenuModel};

        fn model(groups: &[(&str, &str)]) -> TrayMenuModel {
            TrayMenuModel {
                status: "ready".into(),
                connection_label: "connect".into(),
                connection_enabled: true,
                groups: groups
                    .iter()
                    .map(|(id, label)| TrayMenuEntry {
                        label: (*label).into(),
                        checked: false,
                        enabled: true,
                        command: TrayCommand::SelectGroup {
                            group_id: (*id).into(),
                        },
                    })
                    .collect(),
                nodes: Vec::new(),
            }
        }

        #[test]
        fn stale_route_event_does_not_select_a_different_reordered_route() {
            let (_, old_commands) =
                native_menu(&model(&[("alpha", "Alpha"), ("beta", "Beta")])).unwrap();
            let stale_alpha_event = old_commands
                .iter()
                .find_map(|(id, command)| {
                    (command
                        == &TrayCommand::SelectGroup {
                            group_id: "alpha".into(),
                        })
                        .then(|| id.clone())
                })
                .unwrap();

            let (_, reordered_commands) =
                native_menu(&model(&[("beta", "Beta"), ("alpha", "Alpha")])).unwrap();

            assert_ne!(
                reordered_commands.get(&stale_alpha_event),
                Some(&TrayCommand::SelectGroup {
                    group_id: "beta".into(),
                })
            );
            assert_eq!(
                reordered_commands.get(&stale_alpha_event),
                Some(&TrayCommand::SelectGroup {
                    group_id: "alpha".into(),
                })
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use native::start;

#[cfg(not(windows))]
pub(crate) struct TrayRuntime;

#[cfg(not(windows))]
pub(crate) fn start<Model, Action>(
    _initial: TrayMenuModel,
    _model_provider: Model,
    _action_handler: Action,
) -> Result<TrayRuntime, String>
where
    Model: Fn() -> TrayMenuModel + 'static,
    Action: Fn(TrayCommand) + 'static,
{
    Err("system tray is available only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::{TrayCommand, TrayGroup, TrayInput, build_menu_model};

    #[test]
    fn menu_exposes_contextual_connection_and_current_group_nodes() {
        let input = TrayInput {
            status: "В сети".into(),
            connection_label: "Отключиться".into(),
            connection_enabled: true,
            groups: vec![
                TrayGroup {
                    id: "server:opaque".into(),
                    label: "Сервер".into(),
                    selected: true,
                },
                TrayGroup {
                    id: "games:opaque".into(),
                    label: "Игры".into(),
                    selected: false,
                },
            ],
            selected_group_id: Some("server:opaque".into()),
            nodes: vec![("node:a".into(), "Швеция".into(), true)],
            route_selection_enabled: true,
        };

        let model = build_menu_model(input);
        assert_eq!(model.status, "В сети");
        assert_eq!(model.connection_label, "Отключиться");
        assert_eq!(model.groups.len(), 2);
        assert_eq!(model.nodes.len(), 1);
        assert_eq!(
            model.nodes[0].command,
            TrayCommand::SelectNode {
                group_id: "server:opaque".into(),
                node_id: "node:a".into(),
            }
        );
    }

    #[test]
    fn nodes_are_disabled_without_a_current_group() {
        let model = build_menu_model(TrayInput {
            status: "Готово".into(),
            connection_label: "Подключиться".into(),
            connection_enabled: false,
            groups: Vec::new(),
            selected_group_id: None,
            nodes: vec![("stale".into(), "Старый".into(), false)],
            route_selection_enabled: true,
        });

        assert!(model.nodes.is_empty());
        assert!(!model.connection_enabled);
    }

    #[test]
    fn native_tray_keeps_context_menu_on_both_mouse_buttons() {
        let source = include_str!("tray.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains(".with_menu_on_left_click(true)"));
        assert!(production.contains("TrayIconEvent::DoubleClick"));
    }
}
