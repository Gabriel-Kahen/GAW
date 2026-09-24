use super::{BAND_COLORS, GawApp, ParametricEqParameters, ProcessorKind, egui};
use crate::{demo_project, model::Intent, settings::AudioPreferences};
use gaw_core::ProcessorStack;

struct Editor {
    context: egui::Context,
    app: GawApp,
    size: egui::Vec2,
    time: f64,
}

impl Editor {
    fn new(width: f32) -> Self {
        let context = egui::Context::default();
        let app =
            GawApp::with_project_runtime(&context, demo_project(), AudioPreferences::default())
                .unwrap();
        Self {
            context,
            app,
            size: egui::vec2(width, 760.0),
            time: 0.0,
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) -> Vec<egui::epaint::ClippedShape> {
        self.time += 1.0 / 60.0;
        let context = self.context.clone();
        context
            .run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.size)),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    self.app.handle_keyboard(ui.ctx(), self.time);
                    egui::Panel::bottom("eq_test_context")
                        .exact_size(150.0)
                        .show_inside(ui, |ui| self.app.context_editor(ui));
                    ui.label("TIMELINE WORKSPACE");
                    self.app.equalizer_window(ui.ctx());
                },
            )
            .shapes
    }

    fn settled(&mut self) -> Vec<egui::epaint::ClippedShape> {
        // Let the native floating-window animation reach stable geometry and colors.
        for _ in 0..11 {
            self.frame(Vec::new());
        }
        self.frame(Vec::new())
    }

    fn pointer_button(&mut self, pos: egui::Pos2, pressed: bool) {
        self.frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }

    fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
        self.pointer_button(from, true);
        // The first move deliberately leaves the node hit radius, as a fast mouse motion does.
        for fraction in [0.7, 0.85, 1.0] {
            self.frame(vec![egui::Event::PointerMoved(
                from + (to - from) * fraction,
            )]);
        }
        self.pointer_button(to, false);
    }

    fn parameters(&self) -> ParametricEqParameters {
        let ProcessorKind::ParametricEq(parameters) =
            self.app.vm.selected_processor().unwrap().kind
        else {
            panic!("graphical EQ must retain its processor selection");
        };
        parameters
    }
}

fn visit(shapes: &[egui::epaint::ClippedShape], mut inspect: impl FnMut(&egui::Shape, egui::Rect)) {
    fn walk(
        shape: &egui::Shape,
        clip: egui::Rect,
        inspect: &mut impl FnMut(&egui::Shape, egui::Rect),
    ) {
        if let egui::Shape::Vec(children) = shape {
            for child in children {
                walk(child, clip, inspect);
            }
        } else {
            inspect(shape, clip);
        }
    }
    for shape in shapes {
        walk(&shape.shape, shape.clip_rect, &mut inspect);
    }
}

fn node(shapes: &[egui::epaint::ClippedShape], band: usize) -> egui::Pos2 {
    let mut center = None;
    visit(shapes, |shape, clip| {
        if let egui::Shape::Circle(circle) = shape
            && (circle.radius - 7.5).abs() < 0.01
            && circle.stroke.color == BAND_COLORS[band]
            && clip.contains(circle.center)
        {
            center = Some(circle.center);
        }
    });
    center.unwrap_or_else(|| panic!("band {} must have a visible colored graph node", band + 1))
}

fn text_rect(shapes: &[egui::epaint::ClippedShape], label: &str) -> Option<egui::Rect> {
    let mut rect = None;
    visit(shapes, |shape, clip| {
        if let egui::Shape::Text(text) = shape
            && text.galley.job.text == label
            && clip.contains_rect(text.visual_bounding_rect())
        {
            rect = Some(text.visual_bounding_rect());
        }
    });
    rect
}

fn text_containing_rect(
    shapes: &[egui::epaint::ClippedShape],
    fragment: &str,
) -> Option<egui::Rect> {
    let mut rect = None;
    visit(shapes, |shape, clip| {
        if let egui::Shape::Text(text) = shape
            && text.galley.job.text.contains(fragment)
            && clip.intersects(text.visual_bounding_rect())
        {
            rect = Some(text.visual_bounding_rect());
        }
    });
    rect
}

#[test]
fn floating_eq_leaves_the_chin_on_clip_context_and_closes_without_an_edit() {
    let mut editor = Editor::new(1_024.0);
    let scope = editor.app.vm.clip_stack(0, 0).unwrap();
    editor.app.vm.open_equalizer(scope);
    let original = editor.app.vm.project().clone();
    let revision = editor.app.vm.revision();

    let shapes = editor.settled();
    assert!(text_containing_rect(&shapes, "PARAMETRIC EQ").is_some());
    assert!(
        text_rect(&shapes, "WAVEFORM").is_some(),
        "the bottom context panel remains on the clip waveform"
    );
    assert!(text_rect(&shapes, "TIMELINE WORKSPACE").is_some());

    editor.frame(vec![egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }]);
    let shapes = editor.settled();
    assert!(text_containing_rect(&shapes, "PARAMETRIC EQ").is_none());
    assert!(text_rect(&shapes, "WAVEFORM").is_some());
    assert_eq!(editor.app.vm.project(), &original);
    assert_eq!(editor.app.vm.revision(), revision);
}

#[test]
fn graph_drag_edits_its_scope_and_each_gesture_is_one_undo() {
    let fixture = Editor::new(1024.0);
    let scopes = [
        fixture.app.vm.clip_stack(0, 0).unwrap(),
        ProcessorStack::Track {
            track_id: fixture.app.vm.project().tracks[0].id,
        },
        ProcessorStack::CompositionOutput {
            composition_id: fixture.app.vm.project().root_composition_id,
        },
    ];
    for scope in scopes {
        let mut editor = Editor::new(1024.0);
        editor.app.vm.open_equalizer(scope.clone());
        let original = editor.app.vm.project().clone();
        let original_parameters = editor.parameters();
        let shapes = editor.settled();
        let start = node(&shapes, 3);
        editor.drag(start, start + egui::vec2(32.0, -20.0));
        let first_parameters = editor.parameters();
        assert!(first_parameters.bands[3].frequency_hz > original_parameters.bands[3].frequency_hz);
        assert!(first_parameters.bands[3].gain_db > original_parameters.bands[3].gain_db);
        assert_eq!(editor.app.vm.signal_stack(), Some(scope.clone()));
        for index in [0, 1, 2, 4, 5, 6, 7] {
            assert_eq!(
                first_parameters.bands[index],
                original_parameters.bands[index]
            );
        }
        let first = editor.app.vm.project().clone();
        let shapes = editor.settled();
        let start = node(&shapes, 3);
        editor.drag(start, start + egui::vec2(24.0, -12.0));
        let second = editor.app.vm.project().clone();
        assert_ne!(first, second);
        editor.app.vm.apply(Intent::Undo(0.0));
        assert_eq!(
            editor.app.vm.project(),
            &first,
            "separate drags must not merge at {scope:?}"
        );
        editor.app.vm.apply(Intent::Undo(0.0));
        assert_eq!(
            editor.app.vm.project(),
            &original,
            "a multi-frame drag must undo atomically at {scope:?}"
        );
        editor.app.vm.apply(Intent::Redo(0.0));
        assert_eq!(editor.app.vm.project(), &first);
        editor.app.vm.apply(Intent::Redo(0.0));
        assert_eq!(editor.app.vm.project(), &second);
    }
}

#[test]
fn floating_panel_keeps_colored_bands_and_controls_reachable_while_resizing() {
    let mut editor = Editor::new(1024.0);
    editor
        .app
        .vm
        .open_equalizer(ProcessorStack::CompositionOutput {
            composition_id: editor.app.vm.project().root_composition_id,
        });
    let original = editor.app.vm.project().clone();
    let revision = editor.app.vm.revision();
    for width in [320.0, 736.0, 1024.0, 320.0] {
        editor.size.x = width;
        let shapes = editor.settled();
        let title = text_containing_rect(&shapes, "PARAMETRIC EQ")
            .expect("the EQ renders as a titled floating window");
        assert!(title.center().y < editor.size.y * 0.4);
        assert!(text_rect(&shapes, "BYPASS EQ").is_some());
        for (band, &color) in BAND_COLORS.iter().enumerate() {
            let point = node(&shapes, band);
            assert!(point.x >= 0.0 && point.x <= width);
            let number =
                text_rect(&shapes, &(band + 1).to_string()).expect("node numbers remain visible");
            assert!(number.expand(2.0).contains(point));
            let mut colored_label = false;
            let expected_color = color;
            visit(&shapes, |shape, clip| {
                if let egui::Shape::Text(text) = shape
                    && text.galley.job.text.starts_with(&format!("{}  ", band + 1))
                    && clip.contains_rect(text.visual_bounding_rect())
                {
                    colored_label = text
                        .galley
                        .job
                        .sections
                        .iter()
                        .any(|section| section.format.color == expected_color);
                }
            });
            if ![0, 7].contains(&band) && (width >= 736.0 || band == 1) {
                assert!(
                    colored_label,
                    "visible band {} needs its matching colored label at width {width}",
                    band + 1
                );
            }
        }
        let mut visible = shapes;
        // At narrow widths the panel scrolls to keep exact controls reachable.
        for _ in 0..12 {
            if text_rect(&visible, "Q / WIDTH").is_some()
                && (width < 736.0 || text_rect(&visible, "REMOVE BAND").is_some())
                && text_containing_rect(&visible, "OUTPUT").is_some()
            {
                break;
            }
            visible = editor.frame(vec![
                // At the narrowest width the scroll area's top is below y=200.
                egui::Event::PointerMoved(egui::pos2(width * 0.5, 300.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(0.0, -600.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
        assert!(
            text_rect(&visible, "Q / WIDTH").is_some(),
            "Q / WIDTH must be reachable at width {width}"
        );
        if width >= 736.0 {
            assert!(
                text_rect(&visible, "REMOVE BAND").is_some(),
                "REMOVE BAND must be reachable at width {width}"
            );
        }
        assert!(
            text_containing_rect(&visible, "OUTPUT").is_some(),
            "OUTPUT must be reachable at width {width}"
        );
        assert_eq!(editor.app.vm.project(), &original);
        assert_eq!(
            editor.app.vm.revision(),
            revision,
            "resizing and scrolling cannot edit EQ"
        );
        editor.frame(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            phase: egui::TouchPhase::Move,
            delta: egui::vec2(0.0, 1000.0),
            modifiers: egui::Modifiers::NONE,
        }]);
        // Finish wheel smoothing before the next resize.
        for _ in 0..24 {
            editor.frame(Vec::new());
        }
    }
}

#[test]
fn empty_saved_eq_can_add_a_band_from_the_editor() {
    let mut project = demo_project();
    let root = project.root_composition_id;
    let processor = gaw_core::Processor {
        id: gaw_core::ProcessorId::new("empty-eq").unwrap(),
        processor_version: 1,
        enabled: true,
        kind: ProcessorKind::ParametricEq(ParametricEqParameters::default()),
    };
    project
        .compositions
        .iter_mut()
        .find(|composition| composition.id == root)
        .unwrap()
        .output_effects
        .push(processor);
    let context = egui::Context::default();
    let app = GawApp::with_project_runtime(&context, project, AudioPreferences::default()).unwrap();
    let mut editor = Editor {
        context,
        app,
        size: egui::vec2(736.0, 760.0),
        time: 0.0,
    };
    editor
        .app
        .vm
        .open_equalizer(ProcessorStack::CompositionOutput {
            composition_id: root,
        });
    assert!(editor.parameters().bands.is_empty());
    let original = editor.app.vm.project().clone();
    let shapes = editor.settled();
    let add = text_rect(&shapes, "+ BAND")
        .expect("empty EQ offers add band")
        .center();
    editor.pointer_button(add, true);
    editor.pointer_button(add, false);
    assert_eq!(editor.parameters().bands.len(), 1);
    let shapes = editor.settled();
    node(&shapes, 0);
    editor.app.vm.apply(Intent::Undo(0.0));
    assert_eq!(editor.app.vm.project(), &original);
}

#[test]
fn response_paint_stays_finite_when_saved_bands_exceed_nyquist() {
    for rate in [44_100, 16_000] {
        let mut project = demo_project();
        project.sample_rate = gaw_core::SampleRate::new(rate).unwrap();
        let root = project.root_composition_id;
        let context = egui::Context::default();
        let app =
            GawApp::with_project_runtime(&context, project, AudioPreferences::default()).unwrap();
        let mut editor = Editor {
            context,
            app,
            size: egui::vec2(1024.0, 760.0),
            time: 0.0,
        };
        editor
            .app
            .vm
            .open_equalizer(ProcessorStack::CompositionOutput {
                composition_id: root,
            });
        let mut parameters = editor.parameters();
        parameters.bands[6].frequency_hz = 24_000.0;
        parameters.bands[6].gain_db = 12.0;
        editor.app.vm.set_selected_eq_parameters(parameters);
        let original = editor.app.vm.project().clone();
        let shapes = editor.settled();
        let mut curves = 0;
        visit(&shapes, |shape, _| {
            if let egui::Shape::Path(path) = shape
                && path.points.len() > 80
            {
                curves += 1;
                assert!(
                    path.points
                        .iter()
                        .all(|point| point.x.is_finite() && point.y.is_finite()),
                    "response coordinates must remain finite at {rate} Hz"
                );
            }
        });
        assert!(
            curves >= 7,
            "individual active bands and combined response must paint"
        );
        assert_eq!(editor.app.vm.project(), &original);
    }
}

#[test]
fn hiding_editor_during_drag_does_not_move_a_band_on_later_empty_drag() {
    let mut editor = Editor::new(1024.0);
    let scope = ProcessorStack::Track {
        track_id: editor.app.vm.project().tracks[0].id,
    };
    editor.app.vm.open_equalizer(scope.clone());
    let shapes = editor.settled();
    let start = node(&shapes, 3);
    editor.pointer_button(start, true);
    editor.frame(vec![egui::Event::PointerMoved(
        start + egui::vec2(25.0, -20.0),
    )]);
    let edited = editor.app.vm.project().clone();
    editor.frame(vec![egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }]);
    assert!(
        editor.app.vm.selected_processor().is_none(),
        "Escape hides the EQ editor"
    );
    editor.pointer_button(start + egui::vec2(25.0, -20.0), false);
    editor.app.vm.open_equalizer(scope);
    let shapes = editor.settled();
    let empty = node(&shapes, 3) + egui::vec2(0.0, 36.0);
    editor.drag(empty, empty + egui::vec2(42.0, 5.0));
    assert_eq!(
        editor.app.vm.project(),
        &edited,
        "an empty graph drag cannot inherit the earlier grabbed band"
    );
}
