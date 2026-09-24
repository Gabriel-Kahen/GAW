//! Focused physical key input, before egui discards modifier keys or translates shortcuts.
use std::{cell::RefCell, collections::HashSet, rc::Rc};

use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, ElementState, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
    window::WindowId,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhysicalPianoKey {
    Key(egui::Key),
    CapsLock,
    ShiftLeft,
    ShiftRight,
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalPianoEvent {
    pub key: PhysicalPianoKey,
    pub pressed: bool,
    pub repeat: bool,
}

#[derive(Clone, Debug, Default)]
struct Capture {
    enabled: bool,
    held: HashSet<PhysicalPianoKey>,
    events: Vec<PhysicalPianoEvent>,
}

impl Capture {
    fn key(
        &mut self,
        key: PhysicalPianoKey,
        pressed: bool,
        repeat: bool,
        modifiers: ModifiersState,
    ) -> bool {
        let released = !pressed && self.held.remove(&key);
        let command = modifiers
            .intersects(ModifiersState::CONTROL | ModifiersState::SUPER | ModifiersState::ALT);
        let hyper = self.held.contains(&PhysicalPianoKey::CapsLock);
        if released || (self.enabled && (!command || hyper || key == PhysicalPianoKey::CapsLock)) {
            if pressed {
                self.held.insert(key);
            }
            if self.enabled {
                self.events.push(PhysicalPianoEvent {
                    key,
                    pressed,
                    repeat,
                });
            }
            return true;
        }
        false
    }
}

fn capture_id() -> egui::Id {
    egui::Id::new("gaw.physical_keyboard")
}

pub fn set_capture(context: &egui::Context, enabled: bool) {
    context.data_mut(|data| {
        let capture = data.get_temp_mut_or_default::<Capture>(capture_id());
        capture.enabled = enabled;
        if !enabled {
            capture.events.clear();
        }
    });
}

pub fn take_events(context: &egui::Context) -> Vec<PhysicalPianoEvent> {
    context.data_mut(|data| {
        std::mem::take(&mut data.get_temp_mut_or_default::<Capture>(capture_id()).events)
    })
}

fn piano_key(code: KeyCode) -> Option<PhysicalPianoKey> {
    use PhysicalPianoKey as P;
    use egui::Key as E;
    Some(match code {
        KeyCode::CapsLock => P::CapsLock,
        KeyCode::ShiftLeft => P::ShiftLeft,
        KeyCode::ShiftRight => P::ShiftRight,
        _ => P::Key(match code {
            KeyCode::Backquote => E::Backtick,
            KeyCode::Digit1 => E::Num1,
            KeyCode::Digit2 => E::Num2,
            KeyCode::Digit3 => E::Num3,
            KeyCode::Digit4 => E::Num4,
            KeyCode::Digit5 => E::Num5,
            KeyCode::Digit6 => E::Num6,
            KeyCode::Digit7 => E::Num7,
            KeyCode::Digit8 => E::Num8,
            KeyCode::Digit9 => E::Num9,
            KeyCode::Digit0 => E::Num0,
            KeyCode::Minus => E::Minus,
            KeyCode::Tab => E::Tab,
            KeyCode::KeyQ => E::Q,
            KeyCode::KeyW => E::W,
            KeyCode::KeyE => E::E,
            KeyCode::KeyR => E::R,
            KeyCode::KeyT => E::T,
            KeyCode::KeyY => E::Y,
            KeyCode::KeyU => E::U,
            KeyCode::KeyI => E::I,
            KeyCode::KeyO => E::O,
            KeyCode::KeyP => E::P,
            KeyCode::BracketLeft => E::OpenBracket,
            KeyCode::KeyA => E::A,
            KeyCode::KeyS => E::S,
            KeyCode::KeyD => E::D,
            KeyCode::KeyF => E::F,
            KeyCode::KeyG => E::G,
            KeyCode::KeyH => E::H,
            KeyCode::KeyJ => E::J,
            KeyCode::KeyK => E::K,
            KeyCode::KeyL => E::L,
            KeyCode::Semicolon => E::Semicolon,
            KeyCode::Quote => E::Quote,
            KeyCode::KeyZ => E::Z,
            KeyCode::KeyX => E::X,
            KeyCode::KeyC => E::C,
            KeyCode::KeyV => E::V,
            KeyCode::KeyB => E::B,
            KeyCode::KeyN => E::N,
            KeyCode::KeyM => E::M,
            KeyCode::Comma => E::Comma,
            KeyCode::Period => E::Period,
            KeyCode::Slash => E::Slash,
            _ => return None,
        }),
    })
}

type RootContext = Rc<RefCell<Option<(egui::Context, WindowId, bool)>>>;

struct KeyboardApplication<'a> {
    inner: eframe::EframeWinitApplication<'a>,
    root: RootContext,
    focused: bool,
    modifiers: ModifiersState,
}

impl ApplicationHandler<eframe::UserEvent> for KeyboardApplication<'_> {
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some((context, root_id, _)) = self.root.borrow().as_ref()
            && *root_id == window_id
        {
            match &event {
                WindowEvent::Focused(focused) => {
                    self.focused = *focused;
                    if !focused {
                        self.modifiers = ModifiersState::empty();
                        context.data_mut(|data| {
                            data.insert_temp(capture_id(), Capture::default());
                        });
                    }
                }
                WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
                WindowEvent::KeyboardInput {
                    event,
                    is_synthetic,
                    ..
                } if self.focused && !is_synthetic => {
                    if context.text_edit_focused() {
                        set_capture(context, false);
                    }
                    if let PhysicalKey::Code(code) = event.physical_key
                        && let Some(key) = piano_key(code)
                    {
                        let consumed = context.data_mut(|data| {
                            data.get_temp_mut_or_default::<Capture>(capture_id()).key(
                                key,
                                event.state == ElementState::Pressed,
                                event.repeat,
                                self.modifiers,
                            )
                        });
                        if consumed {
                            context.request_repaint();
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
        self.inner.window_event(event_loop, window_id, event);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.resumed(event_loop);
        if let Some((_, _, focused)) = self.root.borrow().as_ref() {
            self.focused = *focused;
        }
    }
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        self.inner.new_events(event_loop, cause);
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: eframe::UserEvent) {
        self.inner.user_event(event_loop, event);
    }
    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) {
        self.inner.device_event(event_loop, device_id, event);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);
    }
    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.exiting(event_loop);
    }
    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.memory_warning(event_loop);
    }
}

/// Runs the native app with physical key support scoped to its focused root window.
///
/// # Errors
/// Returns an error if the event loop cannot be created or run.
pub fn run_native(
    app_name: &str,
    mut options: eframe::NativeOptions,
    creator: eframe::AppCreator<'_>,
) -> eframe::Result {
    let mut builder = EventLoop::<eframe::UserEvent>::with_user_event();
    if let Some(hook) = options.event_loop_builder.take() {
        hook(&mut builder);
    }
    let event_loop = builder.build()?;
    let root: RootContext = Rc::default();
    let creation_root = Rc::clone(&root);
    let inner = eframe::create_native(
        app_name,
        options,
        Box::new(move |context| {
            if let Some(window) = context.winit_window() {
                *creation_root.borrow_mut() =
                    Some((context.egui_ctx.clone(), window.id(), window.has_focus()));
            }
            creator(context)
        }),
        &event_loop,
    );
    event_loop.run_app(&mut KeyboardApplication {
        inner,
        root,
        focused: false,
        modifiers: ModifiersState::empty(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_modifier_positions_separately_and_leaves_non_grid_keys_alone() {
        assert_eq!(
            piano_key(KeyCode::CapsLock),
            Some(PhysicalPianoKey::CapsLock)
        );
        assert_eq!(
            piano_key(KeyCode::ShiftLeft),
            Some(PhysicalPianoKey::ShiftLeft)
        );
        assert_eq!(
            piano_key(KeyCode::ShiftRight),
            Some(PhysicalPianoKey::ShiftRight)
        );
        assert_eq!(
            piano_key(KeyCode::Quote),
            Some(PhysicalPianoKey::Key(egui::Key::Quote))
        );
        assert_eq!(piano_key(KeyCode::SuperLeft), None);
        assert_eq!(piano_key(KeyCode::Escape), None);
    }

    #[test]
    fn captures_physical_modifiers_and_chords_without_shortcuts() {
        let mut capture = Capture {
            enabled: true,
            ..Capture::default()
        };
        let none = ModifiersState::empty();
        assert!(capture.key(PhysicalPianoKey::ShiftLeft, true, false, none));
        assert!(capture.key(
            PhysicalPianoKey::ShiftRight,
            true,
            false,
            ModifiersState::SHIFT
        ));
        assert!(capture.key(PhysicalPianoKey::CapsLock, true, false, none));
        assert!(capture.key(
            PhysicalPianoKey::Key(egui::Key::C),
            true,
            false,
            ModifiersState::all()
        ));
        assert_eq!(capture.events.len(), 4);
        assert!(capture.key(
            PhysicalPianoKey::CapsLock,
            false,
            false,
            ModifiersState::all()
        ));
        assert!(capture.key(
            PhysicalPianoKey::Key(egui::Key::C),
            false,
            false,
            ModifiersState::SUPER
        ));
    }

    #[test]
    fn regular_shortcuts_and_inactive_input_pass_through() {
        let mut capture = Capture::default();
        let key = PhysicalPianoKey::Key(egui::Key::K);
        assert!(!capture.key(key, true, false, ModifiersState::empty()));
        capture.enabled = true;
        assert!(!capture.key(key, true, false, ModifiersState::CONTROL));
        assert!(!capture.key(key, true, false, ModifiersState::SUPER));
        assert!(capture.events.is_empty());
    }

    #[test]
    fn disable_discards_events_but_consumes_previously_captured_releases() {
        let context = egui::Context::default();
        set_capture(&context, true);
        context.data_mut(|data| {
            data.get_temp_mut_or_default::<Capture>(capture_id()).key(
                PhysicalPianoKey::CapsLock,
                true,
                false,
                ModifiersState::empty(),
            );
        });
        set_capture(&context, false);
        assert!(take_events(&context).is_empty());
        context.data_mut(|data| {
            let capture = data.get_temp_mut_or_default::<Capture>(capture_id());
            assert!(capture.key(
                PhysicalPianoKey::CapsLock,
                false,
                false,
                ModifiersState::empty()
            ));
            assert!(capture.events.is_empty());
        });
    }
}
