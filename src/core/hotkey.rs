#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub control: bool,
    pub option: bool,
    pub command: bool,
    pub shift: bool,
}

impl Modifiers {
    const fn matches(self, required: Self) -> bool {
        self.control == required.control
            && self.option == required.option
            && self.command == required.command
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    Space,
    Escape,
    Return,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shortcut {
    #[default]
    ControlOptionTab,
    CommandTab,
    ControlCommandTab,
    ControlOptionSpace,
    CommandOptionSpace,
}

impl Shortcut {
    pub const ALL: [Self; 5] = [
        Self::ControlOptionTab,
        Self::CommandTab,
        Self::ControlCommandTab,
        Self::ControlOptionSpace,
        Self::CommandOptionSpace,
    ];

    #[must_use]
    pub const fn code(self) -> isize {
        match self {
            Self::ControlOptionTab => 0,
            Self::ControlCommandTab => 1,
            Self::ControlOptionSpace => 2,
            Self::CommandOptionSpace => 3,
            Self::CommandTab => 4,
        }
    }

    #[must_use]
    pub const fn from_code(code: isize) -> Self {
        match code {
            1 => Self::ControlCommandTab,
            2 => Self::ControlOptionSpace,
            3 => Self::CommandOptionSpace,
            4 => Self::CommandTab,
            _ => Self::ControlOptionTab,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ControlOptionTab => "Control + Option + Tab",
            Self::CommandTab => "Command + Tab",
            Self::ControlCommandTab => "Control + Command + Tab",
            Self::ControlOptionSpace => "Control + Option + Space",
            Self::CommandOptionSpace => "Command + Option + Space",
        }
    }

    const fn modifiers(self) -> Modifiers {
        match self {
            Self::ControlOptionTab | Self::ControlOptionSpace => Modifiers {
                control: true,
                option: true,
                command: false,
                shift: false,
            },
            Self::ControlCommandTab => Modifiers {
                control: true,
                option: false,
                command: true,
                shift: false,
            },
            Self::CommandTab => Modifiers {
                control: false,
                option: false,
                command: true,
                shift: false,
            },
            Self::CommandOptionSpace => Modifiers {
                control: false,
                option: true,
                command: true,
                shift: false,
            },
        }
    }

    pub(crate) const fn matches_modifiers(self, modifiers: Modifiers) -> bool {
        modifiers.matches(self.modifiers())
    }

    const fn matches_trigger(self, key: Key) -> bool {
        matches!(
            (self, key),
            (
                Self::ControlOptionTab | Self::CommandTab | Self::ControlCommandTab,
                Key::Tab
            ) | (
                Self::ControlOptionSpace | Self::CommandOptionSpace,
                Key::Space
            )
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    FlagsChanged(Modifiers),
    KeyDown {
        key: Key,
        modifiers: Modifiers,
        is_repeat: bool,
    },
    TapDisabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PreWarm,
    Show,
    ShowPrevious,
    Next,
    Previous,
    Left,
    Right,
    Up,
    Down,
    First,
    Last,
    Confirm,
    Cancel,
    ReenableTap,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub action: Option<Action>,
    pub consume: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HotkeyState {
    active: bool,
    modifiers_were_down: bool,
}

impl HotkeyState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        self.active
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Advances the hotkey state without allocating. Passing events remain
    /// borrowed by the platform callback; this core never retains them.
    pub fn handle(&mut self, input: Input) -> Outcome {
        self.handle_with_shortcut(input, Shortcut::default())
    }

    pub fn handle_with_shortcut(&mut self, input: Input, shortcut: Shortcut) -> Outcome {
        match input {
            Input::TapDisabled => {
                let action = if self.active {
                    Action::Cancel
                } else {
                    Action::ReenableTap
                };
                self.reset();
                Outcome {
                    action: Some(action),
                    consume: true,
                }
            }
            Input::FlagsChanged(modifiers) => {
                let down = shortcut.matches_modifiers(modifiers);
                let action = if !self.modifiers_were_down && down {
                    Some(Action::PreWarm)
                } else if self.modifiers_were_down && !down && self.active {
                    self.active = false;
                    Some(Action::Confirm)
                } else {
                    None
                };
                self.modifiers_were_down = down;
                Outcome {
                    action,
                    consume: false,
                }
            }
            Input::KeyDown {
                key,
                modifiers,
                is_repeat,
            } if shortcut.matches_modifiers(modifiers) => {
                self.modifiers_were_down = true;
                if shortcut.matches_trigger(key) && is_repeat {
                    return Outcome {
                        action: None,
                        consume: true,
                    };
                }
                match (shortcut.matches_trigger(key), key) {
                    (true, _) if !self.active => {
                        self.active = true;
                        Outcome {
                            action: Some(if modifiers.shift {
                                Action::ShowPrevious
                            } else {
                                Action::Show
                            }),
                            consume: true,
                        }
                    }
                    (true, _) => Outcome {
                        action: Some(if modifiers.shift {
                            Action::Previous
                        } else {
                            Action::Next
                        }),
                        consume: true,
                    },
                    (false, Key::Escape) if self.active => {
                        self.active = false;
                        Outcome {
                            action: Some(Action::Cancel),
                            consume: true,
                        }
                    }
                    (false, Key::Return) if self.active => {
                        self.active = false;
                        Outcome {
                            action: Some(Action::Confirm),
                            consume: true,
                        }
                    }
                    (false, Key::Left) if self.active => Outcome {
                        action: Some(Action::Left),
                        consume: true,
                    },
                    (false, Key::Right) if self.active => Outcome {
                        action: Some(Action::Right),
                        consume: true,
                    },
                    (false, Key::Up) if self.active => Outcome {
                        action: Some(Action::Up),
                        consume: true,
                    },
                    (false, Key::Down) if self.active => Outcome {
                        action: Some(Action::Down),
                        consume: true,
                    },
                    (false, Key::Home) if self.active => Outcome {
                        action: Some(Action::First),
                        consume: true,
                    },
                    (false, Key::End) if self.active => Outcome {
                        action: Some(Action::Last),
                        consume: true,
                    },
                    _ => Outcome::default(),
                }
            }
            Input::KeyDown { .. } => Outcome::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHORD: Modifiers = Modifiers {
        control: true,
        option: true,
        command: false,
        shift: false,
    };

    #[test]
    fn complete_keyboard_cycle() {
        let mut state = HotkeyState::default();
        assert_eq!(
            state.handle(Input::FlagsChanged(CHORD)),
            Outcome {
                action: Some(Action::PreWarm),
                consume: false
            }
        );
        assert_eq!(
            state
                .handle(Input::KeyDown {
                    key: Key::Tab,
                    modifiers: CHORD,
                    is_repeat: false,
                })
                .action,
            Some(Action::Show)
        );
        assert_eq!(
            state
                .handle(Input::KeyDown {
                    key: Key::Tab,
                    modifiers: CHORD,
                    is_repeat: false,
                })
                .action,
            Some(Action::Next)
        );
        assert_eq!(
            state
                .handle(Input::FlagsChanged(Modifiers::default()))
                .action,
            Some(Action::Confirm)
        );
        assert!(!state.is_active());
    }

    #[test]
    fn command_disables_the_chord() {
        let mut state = HotkeyState::default();
        let with_command = Modifiers {
            command: true,
            ..CHORD
        };
        assert_eq!(
            state.handle(Input::KeyDown {
                key: Key::Tab,
                modifiers: with_command,
                is_repeat: false,
            }),
            Outcome::default()
        );
    }

    #[test]
    fn configured_space_shortcut_replaces_the_default() {
        let mut state = HotkeyState::default();
        assert_eq!(
            state.handle_with_shortcut(
                Input::KeyDown {
                    key: Key::Tab,
                    modifiers: CHORD,
                    is_repeat: false,
                },
                Shortcut::ControlOptionSpace,
            ),
            Outcome::default()
        );
        assert_eq!(
            state
                .handle_with_shortcut(
                    Input::KeyDown {
                        key: Key::Space,
                        modifiers: CHORD,
                        is_repeat: false,
                    },
                    Shortcut::ControlOptionSpace,
                )
                .action,
            Some(Action::Show)
        );
    }

    #[test]
    fn every_persisted_shortcut_completes_a_switch_cycle() {
        for shortcut in Shortcut::ALL {
            assert_eq!(Shortcut::from_code(shortcut.code()), shortcut);
            let modifiers = shortcut.modifiers();
            let trigger = match shortcut {
                Shortcut::ControlOptionTab | Shortcut::CommandTab | Shortcut::ControlCommandTab => {
                    Key::Tab
                }
                Shortcut::ControlOptionSpace | Shortcut::CommandOptionSpace => Key::Space,
            };
            let mut state = HotkeyState::default();
            assert_eq!(
                state
                    .handle_with_shortcut(Input::FlagsChanged(modifiers), shortcut)
                    .action,
                Some(Action::PreWarm)
            );
            assert_eq!(
                state
                    .handle_with_shortcut(
                        Input::KeyDown {
                            key: trigger,
                            modifiers,
                            is_repeat: false,
                        },
                        shortcut,
                    )
                    .action,
                Some(Action::Show)
            );
            assert_eq!(
                state
                    .handle_with_shortcut(Input::FlagsChanged(Modifiers::default()), shortcut)
                    .action,
                Some(Action::Confirm)
            );
        }
    }

    #[test]
    fn command_tab_can_replace_the_system_switcher_and_reverse_with_shift() {
        let shortcut = Shortcut::CommandTab;
        let modifiers = shortcut.modifiers();
        let mut state = HotkeyState::default();
        assert_eq!(
            state
                .handle_with_shortcut(
                    Input::KeyDown {
                        key: Key::Tab,
                        modifiers,
                        is_repeat: false,
                    },
                    shortcut,
                )
                .action,
            Some(Action::Show)
        );
        let reverse = Modifiers {
            shift: true,
            ..modifiers
        };
        assert_eq!(
            state.handle_with_shortcut(
                Input::KeyDown {
                    key: Key::Tab,
                    modifiers: reverse,
                    is_repeat: false,
                },
                shortcut,
            ),
            Outcome {
                action: Some(Action::Previous),
                consume: true,
            }
        );
        assert_eq!(
            state
                .handle_with_shortcut(Input::FlagsChanged(Modifiers::default()), shortcut)
                .action,
            Some(Action::Confirm)
        );
    }

    #[test]
    fn first_shift_trigger_opens_in_reverse_direction() {
        let shortcut = Shortcut::CommandTab;
        let modifiers = Modifiers {
            shift: true,
            ..shortcut.modifiers()
        };
        assert_eq!(
            HotkeyState::default().handle_with_shortcut(
                Input::KeyDown {
                    key: Key::Tab,
                    modifiers,
                    is_repeat: false,
                },
                shortcut,
            ),
            Outcome {
                action: Some(Action::ShowPrevious),
                consume: true,
            }
        );
    }

    #[test]
    fn disabled_tap_cancels_an_active_overlay_and_resets_state() {
        let mut state = HotkeyState::default();
        let _ = state.handle(Input::KeyDown {
            key: Key::Tab,
            modifiers: CHORD,
            is_repeat: false,
        });
        assert!(state.is_active());
        assert_eq!(
            state.handle(Input::TapDisabled),
            Outcome {
                action: Some(Action::Cancel),
                consume: true,
            }
        );
        assert!(!state.is_active());
    }

    #[test]
    fn trigger_autorepeat_is_consumed_without_skipping_windows() {
        let mut state = HotkeyState::default();
        assert_eq!(
            state.handle(Input::KeyDown {
                key: Key::Tab,
                modifiers: CHORD,
                is_repeat: false,
            }),
            Outcome {
                action: Some(Action::Show),
                consume: true,
            }
        );
        assert_eq!(
            state.handle(Input::KeyDown {
                key: Key::Tab,
                modifiers: CHORD,
                is_repeat: true,
            }),
            Outcome {
                action: None,
                consume: true,
            }
        );
        assert_eq!(
            state.handle(Input::KeyDown {
                key: Key::Tab,
                modifiers: CHORD,
                is_repeat: false,
            }),
            Outcome {
                action: Some(Action::Next),
                consume: true,
            }
        );
    }

    #[test]
    fn grid_navigation_keys_are_consumed_only_while_chooser_is_active() {
        let mut state = HotkeyState::default();
        assert_eq!(
            state.handle(Input::KeyDown {
                key: Key::Right,
                modifiers: CHORD,
                is_repeat: false,
            }),
            Outcome::default()
        );
        let _ = state.handle(Input::KeyDown {
            key: Key::Tab,
            modifiers: CHORD,
            is_repeat: false,
        });
        for (key, action) in [
            (Key::Left, Action::Left),
            (Key::Right, Action::Right),
            (Key::Up, Action::Up),
            (Key::Down, Action::Down),
            (Key::Home, Action::First),
            (Key::End, Action::Last),
        ] {
            assert_eq!(
                state.handle(Input::KeyDown {
                    key,
                    modifiers: CHORD,
                    is_repeat: true,
                }),
                Outcome {
                    action: Some(action),
                    consume: true,
                }
            );
        }
    }
}
