use std::time::{Duration, Instant};

use r86::devices::{Ps2ButtonState, Ps2Key};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use sdl2::pixels::PixelFormatEnum;

use super::{GuestStepStatus, Machine, Sdl2Options, print_machine_stop};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub fn run(
    machine: &mut Machine,
    options: Sdl2Options,
    network: &mut Option<r86::network::UserNetwork>,
    audio: &mut r86::audio_output::AudioOutput,
) -> Result<(), String> {
    let sdl = sdl2::init().map_err(|err| format!("failed to initialize SDL2: {err}"))?;
    let video = sdl
        .video()
        .map_err(|err| format!("failed to initialize SDL2 video: {err}"))?;
    let mouse = sdl.mouse();
    sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");

    let initial = machine.vga().mode_info();
    let window_width = u32::from(initial.width).saturating_mul(options.scale);
    let window_height = u32::from(initial.height).saturating_mul(options.scale);
    let mut canvas = create_canvas(&video, window_width.max(1), window_height.max(1))?;
    canvas
        .set_logical_size(u32::from(initial.width), u32::from(initial.height))
        .map_err(|err| format!("failed to set SDL2 logical size: {err}"))?;
    let texture_creator = canvas.texture_creator();
    let mut event_pump = sdl
        .event_pump()
        .map_err(|err| format!("failed to create SDL2 event pump: {err}"))?;
    let mut frontend = FrontendState {
        mouse_buttons: Ps2ButtonState::default(),
        held_keys: Vec::new(),
        reset_count: machine.reset_count(),
        grabbed: false,
        scale: options.scale,
        tex_width: 0,
        tex_height: 0,
        pixels: Vec::new(),
    };
    set_mouse_grab(
        machine,
        &mut canvas,
        &mouse,
        &mut frontend,
        options.grab_mouse,
    );

    let mut texture = None;
    let mut last_present = Instant::now()
        .checked_sub(FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);
    render(
        machine,
        &mut canvas,
        &texture_creator,
        &mut texture,
        &mut frontend,
    )?;

    loop {
        for event in event_pump.poll_iter() {
            if matches!(event, Event::RenderDeviceReset { .. }) {
                texture = None;
            }
            if handle_event(machine, &mut canvas, &mouse, &mut frontend, event)? {
                return Ok(());
            }
        }

        if let Some(limit) = options.step_limit {
            if machine.step_count() >= limit {
                render(
                    machine,
                    &mut canvas,
                    &texture_creator,
                    &mut texture,
                    &mut frontend,
                )?;
                return Ok(());
            }
        }
        let budget = options
            .step_limit
            .map(|limit| {
                limit
                    .saturating_sub(machine.step_count())
                    .min(options.slice_steps)
            })
            .unwrap_or(options.slice_steps);
        let report = if budget == 0 || (machine.powered_off() && options.do_not_exit_when_poweroff)
        {
            None
        } else {
            if let Some(net) = network {
                net.poll(machine).map_err(|e| format!("network: {e}"))?;
            }
            Some(machine.run_steps(budget)?)
        };
        audio.pump(machine)?;
        if let Some(report) = report {
            if let Some(status) = report.stop {
                match status {
                    GuestStepStatus::Halted => {
                        let wait = machine.halted_wait_duration();
                        std::thread::sleep(
                            network.as_ref().map_or(wait, |net| net.limit_wait(wait)),
                        );
                    }
                    _ => {
                        print_machine_stop(machine, &status);
                        render(
                            machine,
                            &mut canvas,
                            &texture_creator,
                            &mut texture,
                            &mut frontend,
                        )?;
                        if !matches!(status, GuestStepStatus::PoweredOff)
                            || !options.do_not_exit_when_poweroff
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }

        let now = Instant::now();
        if now.duration_since(last_present) >= FRAME_INTERVAL {
            render(
                machine,
                &mut canvas,
                &texture_creator,
                &mut texture,
                &mut frontend,
            )?;
            last_present = now;
        }
        if machine.powered_off() {
            // Keep the final screen and host event handling without repeatedly
            // running the powered-off machine or flushing the same disk.
            std::thread::sleep(FRAME_INTERVAL);
        }
    }
}

fn create_canvas(
    video: &sdl2::VideoSubsystem,
    window_width: u32,
    window_height: u32,
) -> Result<sdl2::render::Canvas<sdl2::video::Window>, String> {
    create_canvas_with_mode(video, window_width, window_height, true)
        .or_else(|_| create_canvas_with_mode(video, window_width, window_height, false))
}

fn create_canvas_with_mode(
    video: &sdl2::VideoSubsystem,
    window_width: u32,
    window_height: u32,
    accelerated: bool,
) -> Result<sdl2::render::Canvas<sdl2::video::Window>, String> {
    let window = video
        .window("r86", window_width, window_height)
        .position_centered()
        .resizable()
        .allow_highdpi()
        .build()
        .map_err(|err| format!("failed to create SDL2 window: {err}"))?;
    let mut builder = window.into_canvas();
    if accelerated {
        builder = builder.accelerated().present_vsync();
    } else {
        builder = builder.software();
    }
    builder
        .build()
        .map_err(|err| format!("failed to create SDL2 renderer: {err}"))
}

fn render<'a>(
    machine: &mut Machine,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    texture_creator: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    texture: &mut Option<sdl2::render::Texture<'a>>,
    frontend: &mut FrontendState,
) -> Result<(), String> {
    if texture.is_none() || machine.vga().whole_frame_dirty() {
        update_texture(machine, canvas, texture_creator, texture, frontend)?;
    }
    // Present the cached texture even when the guest is idle. SDL invalidates
    // the backbuffer after presenting, and the window may have been exposed.
    canvas.clear();
    canvas
        .copy(texture.as_ref().unwrap(), None, None)
        .map_err(|err| format!("failed to draw SDL2 texture: {err}"))?;
    canvas.present();
    Ok(())
}

fn update_texture<'a>(
    machine: &mut Machine,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    texture_creator: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    texture: &mut Option<sdl2::render::Texture<'a>>,
    frontend: &mut FrontendState,
) -> Result<(), String> {
    let info = machine.vga().mode_info();
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let len = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "SDL2 frame dimensions overflow host usize".to_string())?;
    if frontend.pixels.len() != len {
        frontend.pixels.resize(len, 0);
    }
    machine
        .vga()
        .render_rgba(&mut frontend.pixels)
        .ok_or_else(|| "VGA renderer rejected the SDL2 frame buffer".to_string())?;

    let dimensions_changed = frontend.tex_width != width || frontend.tex_height != height;
    if texture.is_none() || dimensions_changed {
        *texture = Some(
            texture_creator
                .create_texture_streaming(PixelFormatEnum::ABGR8888, width, height)
                .map_err(|err| format!("failed to create SDL2 texture: {err}"))?,
        );
    }
    if dimensions_changed {
        frontend.tex_width = width;
        frontend.tex_height = height;
        canvas
            .window_mut()
            .set_size(
                width.saturating_mul(frontend.scale).max(1),
                height.saturating_mul(frontend.scale).max(1),
            )
            .map_err(|err| format!("failed to resize SDL2 window: {err}"))?;
        canvas
            .set_logical_size(width, height)
            .map_err(|err| format!("failed to resize SDL2 logical surface: {err}"))?;
    }
    let texture = texture
        .as_mut()
        .ok_or_else(|| "SDL2 texture was not initialized".to_string())?;
    texture
        .update(None, &frontend.pixels, width as usize * 4)
        .map_err(|err| format!("failed to update SDL2 texture: {err}"))?;
    machine.clear_frame_dirty();
    Ok(())
}

fn handle_event(
    machine: &mut Machine,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    mouse: &sdl2::mouse::MouseUtil,
    frontend: &mut FrontendState,
    event: Event,
) -> Result<bool, String> {
    // A board reset discards device input. Do not resurrect old mouse buttons
    // or send typematic repeats from the previous guest's key ownership.
    if frontend.reset_count != machine.reset_count() {
        frontend.held_keys.clear();
        frontend.mouse_buttons = Ps2ButtonState::default();
        frontend.reset_count = machine.reset_count();
    }
    match event {
        Event::Quit { .. } => {
            release_input(machine, frontend);
            return Ok(true);
        }
        Event::Window {
            win_event: WindowEvent::FocusLost | WindowEvent::Hidden | WindowEvent::Minimized,
            ..
        } => set_mouse_grab(machine, canvas, mouse, frontend, false),
        Event::KeyDown {
            keycode: Some(keycode),
            repeat,
            ..
        } => {
            handle_key(machine, canvas, mouse, frontend, keycode, true, repeat);
        }
        Event::KeyUp {
            keycode: Some(keycode),
            ..
        } => {
            handle_key(machine, canvas, mouse, frontend, keycode, false, false);
        }
        Event::MouseMotion { xrel, yrel, .. } => {
            if frontend.grabbed && (xrel != 0 || yrel != 0) {
                machine.inject_mouse(
                    clamp_i32_to_i16(xrel),
                    clamp_i32_to_i16(yrel),
                    frontend.mouse_buttons,
                );
            }
        }
        Event::MouseButtonDown { mouse_btn, .. } => {
            if !frontend.grabbed {
                set_mouse_grab(machine, canvas, mouse, frontend, true);
            }
            if update_mouse_button(&mut frontend.mouse_buttons, mouse_btn, true) {
                machine.inject_mouse(0, 0, frontend.mouse_buttons);
            }
        }
        Event::MouseButtonUp { mouse_btn, .. } => {
            if update_mouse_button(&mut frontend.mouse_buttons, mouse_btn, false) {
                machine.inject_mouse(0, 0, frontend.mouse_buttons);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_key(
    machine: &mut Machine,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    mouse: &sdl2::mouse::MouseUtil,
    frontend: &mut FrontendState,
    keycode: Keycode,
    down: bool,
    repeat: bool,
) {
    // Track this frontend's pressed keys, excluding consumed shortcuts.
    // tiny386 tracks pressed keys; v86 also releases them on blur and ignores
    // stray keyups. Derive Ctrl from both actual keys instead of a shared
    // boolean that either release clears.
    let held = frontend.held_keys.iter().position(|key| *key == keycode);
    if !down {
        if let Some(index) = held {
            frontend.held_keys.remove(index);
            inject_keycode(machine, keycode, false);
        }
        return;
    }
    if repeat {
        if held.is_some() {
            inject_keycode(machine, keycode, true);
        }
        return;
    }
    if held.is_some() {
        return;
    }
    let ctrl = frontend.held_keys.contains(&Keycode::LCtrl)
        || frontend.held_keys.contains(&Keycode::RCtrl);
    if ctrl && keycode == Keycode::RightBracket {
        set_mouse_grab(machine, canvas, mouse, frontend, !frontend.grabbed);
        return;
    }
    if inject_keycode(machine, keycode, true) {
        frontend.held_keys.push(keycode);
    }
}

fn inject_keycode(machine: &mut Machine, keycode: Keycode, down: bool) -> bool {
    if let Some(key) = ps2_key_from_keycode(keycode) {
        machine.inject_key(key, down);
        true
    } else if let Some(make) = set2_make_from_keycode(keycode) {
        inject_set2_make_break(machine, make, down);
        true
    } else {
        false
    }
}

fn release_input(machine: &mut Machine, frontend: &mut FrontendState) {
    for key in frontend.held_keys.drain(..) {
        inject_keycode(machine, key, false);
    }
    if frontend.mouse_buttons != Ps2ButtonState::default() {
        frontend.mouse_buttons = Ps2ButtonState::default();
        machine.inject_mouse(0, 0, frontend.mouse_buttons);
    }
}

fn set_mouse_grab(
    machine: &mut Machine,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    mouse: &sdl2::mouse::MouseUtil,
    frontend: &mut FrontendState,
    grabbed: bool,
) {
    if !grabbed {
        release_input(machine, frontend);
    }
    frontend.grabbed = grabbed;
    canvas.window_mut().set_grab(grabbed);
    mouse.set_relative_mouse_mode(grabbed);
    mouse.show_cursor(!grabbed);
}

fn update_mouse_button(buttons: &mut Ps2ButtonState, button: MouseButton, down: bool) -> bool {
    match button {
        MouseButton::Left => buttons.left = down,
        MouseButton::Right => buttons.right = down,
        MouseButton::Middle => buttons.middle = down,
        _ => return false,
    }
    true
}

fn ps2_key_from_keycode(keycode: Keycode) -> Option<Ps2Key> {
    let key = match keycode {
        Keycode::Return => Ps2Key::Enter,
        Keycode::Space => Ps2Key::Space,
        Keycode::Backspace => Ps2Key::Backspace,
        Keycode::Tab => Ps2Key::Tab,
        Keycode::Escape => Ps2Key::Escape,
        Keycode::LShift => Ps2Key::LeftShift,
        Keycode::RShift => Ps2Key::RightShift,
        Keycode::A => Ps2Key::A,
        Keycode::B => Ps2Key::B,
        Keycode::C => Ps2Key::C,
        Keycode::D => Ps2Key::D,
        Keycode::E => Ps2Key::E,
        Keycode::F => Ps2Key::F,
        Keycode::G => Ps2Key::G,
        Keycode::H => Ps2Key::H,
        Keycode::I => Ps2Key::I,
        Keycode::J => Ps2Key::J,
        Keycode::K => Ps2Key::K,
        Keycode::L => Ps2Key::L,
        Keycode::M => Ps2Key::M,
        Keycode::N => Ps2Key::N,
        Keycode::O => Ps2Key::O,
        Keycode::P => Ps2Key::P,
        Keycode::Q => Ps2Key::Q,
        Keycode::R => Ps2Key::R,
        Keycode::S => Ps2Key::S,
        Keycode::T => Ps2Key::T,
        Keycode::U => Ps2Key::U,
        Keycode::V => Ps2Key::V,
        Keycode::W => Ps2Key::W,
        Keycode::X => Ps2Key::X,
        Keycode::Y => Ps2Key::Y,
        Keycode::Z => Ps2Key::Z,
        Keycode::Num0 => Ps2Key::Num0,
        Keycode::Num1 => Ps2Key::Num1,
        Keycode::Num2 => Ps2Key::Num2,
        Keycode::Num3 => Ps2Key::Num3,
        Keycode::Num4 => Ps2Key::Num4,
        Keycode::Num5 => Ps2Key::Num5,
        Keycode::Num6 => Ps2Key::Num6,
        Keycode::Num7 => Ps2Key::Num7,
        Keycode::Num8 => Ps2Key::Num8,
        Keycode::Num9 => Ps2Key::Num9,
        Keycode::Minus => Ps2Key::Minus,
        Keycode::Equals => Ps2Key::Equal,
        Keycode::LeftBracket => Ps2Key::LeftBracket,
        Keycode::RightBracket => Ps2Key::RightBracket,
        Keycode::Backslash => Ps2Key::Backslash,
        Keycode::Semicolon => Ps2Key::Semicolon,
        Keycode::Quote => Ps2Key::Apostrophe,
        Keycode::Backquote => Ps2Key::Grave,
        Keycode::Comma => Ps2Key::Comma,
        Keycode::Period => Ps2Key::Dot,
        Keycode::Slash => Ps2Key::Slash,
        _ => return None,
    };
    Some(key)
}

fn set2_make_from_keycode(keycode: Keycode) -> Option<&'static [u8]> {
    match keycode {
        Keycode::LCtrl => Some(&[0x14]),
        Keycode::RCtrl => Some(&[0xe0, 0x14]),
        Keycode::LAlt => Some(&[0x11]),
        Keycode::RAlt => Some(&[0xe0, 0x11]),
        Keycode::CapsLock => Some(&[0x58]),
        Keycode::F1 => Some(&[0x05]),
        Keycode::F2 => Some(&[0x06]),
        Keycode::F3 => Some(&[0x04]),
        Keycode::F4 => Some(&[0x0c]),
        Keycode::F5 => Some(&[0x03]),
        Keycode::F6 => Some(&[0x0b]),
        Keycode::F7 => Some(&[0x83]),
        Keycode::F8 => Some(&[0x0a]),
        Keycode::F9 => Some(&[0x01]),
        Keycode::F10 => Some(&[0x09]),
        Keycode::F11 => Some(&[0x78]),
        Keycode::F12 => Some(&[0x07]),
        Keycode::Up => Some(&[0xe0, 0x75]),
        Keycode::Down => Some(&[0xe0, 0x72]),
        Keycode::Left => Some(&[0xe0, 0x6b]),
        Keycode::Right => Some(&[0xe0, 0x74]),
        Keycode::Home => Some(&[0xe0, 0x6c]),
        Keycode::End => Some(&[0xe0, 0x69]),
        Keycode::PageUp => Some(&[0xe0, 0x7d]),
        Keycode::PageDown => Some(&[0xe0, 0x7a]),
        Keycode::Insert => Some(&[0xe0, 0x70]),
        Keycode::Delete => Some(&[0xe0, 0x71]),
        Keycode::KpEnter => Some(&[0xe0, 0x5a]),
        Keycode::KpDivide => Some(&[0xe0, 0x4a]),
        _ => None,
    }
}

fn inject_set2_make_break(machine: &mut Machine, make: &[u8], down: bool) {
    let mut bytes = [0u8; 4];
    let len = if down {
        bytes[..make.len()].copy_from_slice(make);
        make.len()
    } else if make.first() == Some(&0xe0) && make.len() == 2 {
        bytes[0] = 0xe0;
        bytes[1] = 0xf0;
        bytes[2] = make[1];
        3
    } else if make.len() == 1 {
        bytes[0] = 0xf0;
        bytes[1] = make[0];
        2
    } else {
        0
    };
    if len != 0 {
        machine.inject_scancode_set2(&bytes[..len]);
    }
}

fn clamp_i32_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

struct FrontendState {
    mouse_buttons: Ps2ButtonState,
    held_keys: Vec<Keycode>,
    reset_count: u64,
    grabbed: bool,
    scale: u32,
    tex_width: u32,
    tex_height: u32,
    pixels: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdl2::event::WindowEvent;
    use sdl2::keyboard::Mod;
    use sdl2::mouse::MouseState;

    // Exercise the real SDL queue and frontend against guest IN/OUT, without
    // requiring a desktop. Run with SDL_VIDEODRIVER=dummy and --test-threads=1.
    struct InputProbe {
        machine: Machine,
        canvas: sdl2::render::Canvas<sdl2::video::Window>,
        mouse: sdl2::mouse::MouseUtil,
        frontend: FrontendState,
        events: sdl2::EventSubsystem,
        pump: sdl2::EventPump,
        serial_cursor: usize,
        _sdl: sdl2::Sdl,
    }

    impl InputProbe {
        fn new() -> Self {
            let sdl = sdl2::init().unwrap();
            let video = sdl.video().unwrap();
            let canvas = video
                .window("r86 input check", 320, 200)
                .hidden()
                .build()
                .unwrap()
                .into_canvas()
                .software()
                .build()
                .unwrap();
            let events = sdl.event().unwrap();
            let mut pump = sdl.event_pump().unwrap();
            pump.poll_iter().for_each(drop);
            let mut machine = Machine::new_firmware(2 << 20, r86::FirmwareDisplay::Serial).unwrap();
            machine
                .load_bootsector(include_bytes!("../../tests/bootsectors/ws_ps2_vga.bin"))
                .unwrap();
            machine.enter_bootsector();
            machine.run_steps(20_000).unwrap();
            assert!(String::from_utf8_lossy(machine.serial_output()).ends_with("R86_SYN_READY "));
            let serial_cursor = machine.serial_output().len();
            Self {
                machine,
                canvas,
                mouse: sdl.mouse(),
                frontend: FrontendState {
                    mouse_buttons: Ps2ButtonState::default(),
                    held_keys: Vec::new(),
                    reset_count: 0,
                    grabbed: false,
                    scale: 1,
                    tex_width: 0,
                    tex_height: 0,
                    pixels: Vec::new(),
                },
                events,
                pump,
                serial_cursor,
                _sdl: sdl,
            }
        }

        fn send(&mut self, event: Event) {
            self.events.push_event(event).unwrap();
            for event in self.pump.poll_iter() {
                assert!(
                    !handle_event(
                        &mut self.machine,
                        &mut self.canvas,
                        &self.mouse,
                        &mut self.frontend,
                        event
                    )
                    .unwrap()
                );
            }
            assert!(self.machine.run_steps(10_000).unwrap().stop.is_none());
        }

        fn key(&mut self, keycode: Keycode, down: bool, repeat: bool) {
            let event = if down {
                Event::KeyDown {
                    timestamp: 0,
                    window_id: self.canvas.window().id(),
                    keycode: Some(keycode),
                    scancode: None,
                    keymod: Mod::NOMOD,
                    repeat,
                }
            } else {
                Event::KeyUp {
                    timestamp: 0,
                    window_id: self.canvas.window().id(),
                    keycode: Some(keycode),
                    scancode: None,
                    keymod: Mod::NOMOD,
                    repeat: false,
                }
            };
            self.send(event);
        }

        fn left_down(&mut self) {
            self.send(Event::MouseButtonDown {
                timestamp: 0,
                window_id: self.canvas.window().id(),
                which: 0,
                mouse_btn: MouseButton::Left,
                clicks: 1,
                x: 10,
                y: 10,
            });
        }

        fn serial(&mut self) -> String {
            let bytes = self.machine.serial_output();
            let text = String::from_utf8(bytes[self.serial_cursor..].to_vec()).unwrap();
            self.serial_cursor = bytes.len();
            text
        }
    }

    #[test]
    #[ignore = "SDL event/guest check: SDL_VIDEODRIVER=dummy, --include-ignored --test-threads=1"]
    fn sdl_focus_and_reset_do_not_leave_guest_input_held() {
        let mut p = InputProbe::new();
        p.key(Keycode::LCtrl, true, false);
        p.key(Keycode::A, true, false);
        p.key(Keycode::Home, true, false);
        p.left_down();
        assert_eq!(p.serial(), "K1D K1E KE0 K47 M09 M00 M00 ");
        p.send(Event::Window {
            timestamp: 0,
            window_id: p.canvas.window().id(),
            win_event: WindowEvent::FocusLost,
        });
        let released = p.serial();
        println!("focus release: {released}");
        assert_eq!(released, "K9D K9E KE0 KC7 M08 M00 M00 ");
        assert!(!p.frontend.grabbed);
        // Late keyups/repeats from the old focus must not create orphan bytes.
        p.key(Keycode::LCtrl, false, false);
        p.key(Keycode::A, true, true);
        p.key(Keycode::Home, false, false);
        assert_eq!(p.serial(), "");

        p.key(Keycode::A, true, false);
        p.left_down();
        assert_eq!(p.serial(), "K1E M09 M00 M00 ");
        p.machine.reboot().unwrap();
        p.machine.run_steps(20_000).unwrap();
        assert!(p.serial().ends_with("R86_SYN_READY "));
        p.key(Keycode::A, true, true);
        p.send(Event::MouseMotion {
            timestamp: 0,
            window_id: p.canvas.window().id(),
            which: 0,
            mousestate: MouseState::from_sdl_state(1),
            x: 11,
            y: 10,
            xrel: 1,
            yrel: 0,
        });
        assert_eq!(p.serial(), "M08 M01 M00 ");
    }

    #[test]
    #[ignore = "SDL event/guest check: SDL_VIDEODRIVER=dummy, --include-ignored --test-threads=1"]
    fn sdl_capture_hotkey_preserves_key_pairs_and_both_ctrl_keys() {
        let mut p = InputProbe::new();
        p.key(Keycode::LCtrl, true, false);
        p.key(Keycode::RCtrl, true, false);
        p.key(Keycode::LCtrl, false, false);
        p.key(Keycode::RightBracket, true, false);
        assert!(
            p.frontend.grabbed,
            "right Ctrl still holds the capture shortcut"
        );
        p.key(Keycode::RCtrl, false, false);
        p.key(Keycode::RightBracket, false, false);
        assert_eq!(p.serial(), "K1D KE0 K1D K9D KE0 K9D ");
        // A bracket delivered before Ctrl still needs its own break code.
        p.key(Keycode::RightBracket, true, false);
        p.key(Keycode::LCtrl, true, false);
        p.key(Keycode::RightBracket, false, false);
        p.key(Keycode::LCtrl, false, false);
        assert_eq!(p.serial(), "K1B K1D K9B K9D ");
        p.key(Keycode::A, true, false);
        p.key(Keycode::A, true, true);
        p.left_down();
        p.key(Keycode::LCtrl, true, false);
        assert_eq!(p.serial(), "K1E K1E M09 M00 M00 K1D ");
        p.key(Keycode::RightBracket, true, false);
        let released = p.serial();
        println!("capture release: {released}");
        assert_eq!(released, "K9E K9D M08 M00 M00 ");
        assert!(!p.frontend.grabbed);
        p.key(Keycode::RightBracket, true, true);
        p.key(Keycode::RightBracket, false, false);
        p.key(Keycode::LCtrl, false, false);
        assert_eq!(p.serial(), "");
    }
}
