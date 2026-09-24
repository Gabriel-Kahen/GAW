use super::{PianoKey, egui};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Layout {
    #[default]
    Piano,
    Chromatic,
    SevenEdo,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Pitch {
    pub note: u8,
    pub cents: f64,
}

pub(super) const ROWS: [[(PianoKey, &str); 12]; 4] = {
    use PianoKey::{CapsLock, Key, ShiftLeft, ShiftRight};
    use egui::Key::{
        A, B, Backtick, C, Comma, D, E, F, G, H, I, J, K, L, M, Minus, N, Num0, Num1, Num2, Num3,
        Num4, Num5, Num6, Num7, Num8, Num9, O, OpenBracket, P, Period, Q, Quote, R, S, Semicolon,
        Slash, T, Tab, U, V, W, X, Y, Z,
    };
    [
        [
            (Key(Backtick), "`"),
            (Key(Num1), "1"),
            (Key(Num2), "2"),
            (Key(Num3), "3"),
            (Key(Num4), "4"),
            (Key(Num5), "5"),
            (Key(Num6), "6"),
            (Key(Num7), "7"),
            (Key(Num8), "8"),
            (Key(Num9), "9"),
            (Key(Num0), "0"),
            (Key(Minus), "-"),
        ],
        [
            (Key(Tab), "Tab"),
            (Key(Q), "Q"),
            (Key(W), "W"),
            (Key(E), "E"),
            (Key(R), "R"),
            (Key(T), "T"),
            (Key(Y), "Y"),
            (Key(U), "U"),
            (Key(I), "I"),
            (Key(O), "O"),
            (Key(P), "P"),
            (Key(OpenBracket), "["),
        ],
        [
            (CapsLock, "Hyper"),
            (Key(A), "A"),
            (Key(S), "S"),
            (Key(D), "D"),
            (Key(F), "F"),
            (Key(G), "G"),
            (Key(H), "H"),
            (Key(J), "J"),
            (Key(K), "K"),
            (Key(L), "L"),
            (Key(Semicolon), ";"),
            (Key(Quote), "'"),
        ],
        [
            (ShiftLeft, "LShift"),
            (Key(Z), "Z"),
            (Key(X), "X"),
            (Key(C), "C"),
            (Key(V), "V"),
            (Key(B), "B"),
            (Key(N), "N"),
            (Key(M), "M"),
            (Key(Comma), ","),
            (Key(Period), "."),
            (Key(Slash), "/"),
            (ShiftRight, "RShift"),
        ],
    ]
};

impl Layout {
    pub const ALL: [Self; 3] = [Self::Piano, Self::Chromatic, Self::SevenEdo];

    pub fn label(self) -> &'static str {
        match self {
            Self::Piano => "Piano",
            Self::Chromatic => "4 × 12",
            Self::SevenEdo => "7EDO",
        }
    }

    pub fn columns(self) -> usize {
        if self == Self::SevenEdo { 7 } else { 12 }
    }

    pub fn max_root(self) -> u8 {
        if self == Self::Piano { 108 } else { 72 }
    }

    pub fn pitch(self, key: PianoKey, root: u8) -> Option<Pitch> {
        let semitones = if self == Self::Piano {
            let PianoKey::Key(key) = key else {
                return None;
            };
            f64::from(super::piano_offset(key)?)
        } else {
            let (row, column) = ROWS.iter().enumerate().find_map(|(row, keys)| {
                keys[..self.columns()]
                    .iter()
                    .position(|(candidate, _)| *candidate == key)
                    .map(|column| (row, column))
            })?;
            (3 - row) as f64 * 12.0 + column as f64 * 12.0 / self.columns() as f64
        };
        let pitch = f64::from(root) + semitones;
        let note = pitch.round();
        (note <= 127.0).then_some(Pitch {
            note: note as u8,
            cents: (pitch - note) * 100.0,
        })
    }

    pub fn octave_key(self, key: egui::Key) -> Option<bool> {
        match key {
            egui::Key::PageDown => Some(false),
            egui::Key::PageUp => Some(true),
            egui::Key::Z if self == Self::Piano => Some(false),
            egui::Key::X if self == Self::Piano => Some(true),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromatic_grid_has_exact_requested_rows_and_semitones() {
        assert_eq!(ROWS[0][0].0, PianoKey::Key(egui::Key::Backtick));
        assert_eq!(ROWS[1][0].0, PianoKey::Key(egui::Key::Tab));
        assert_eq!(ROWS[2][0].0, PianoKey::CapsLock);
        assert_eq!(ROWS[3][0].0, PianoKey::ShiftLeft);
        assert_eq!(ROWS[3][11].0, PianoKey::ShiftRight);
        let unique: std::collections::HashSet<_> =
            ROWS.iter().flatten().map(|(key, _)| key).collect();
        assert_eq!(unique.len(), 48);
        for (row, keys) in ROWS.iter().enumerate() {
            for (column, &(key, _)) in keys.iter().enumerate() {
                let pitch = Layout::Chromatic.pitch(key, 36).unwrap();
                assert_eq!(usize::from(pitch.note), 36 + (3 - row) * 12 + column);
                assert!(pitch.cents.abs() < 1e-9);
            }
        }
    }

    #[test]
    fn seven_edo_keeps_equal_steps_and_exact_octaves() {
        for (row, keys) in ROWS.iter().enumerate() {
            for (column, &(key, _)) in keys.iter().enumerate() {
                let pitch = Layout::SevenEdo.pitch(key, 36);
                if column >= 7 {
                    assert!(pitch.is_none());
                    continue;
                }
                let pitch = pitch.unwrap();
                let actual = f64::from(pitch.note) + pitch.cents / 100.0;
                let expected = 36.0 + (3 - row) as f64 * 12.0 + column as f64 * 12.0 / 7.0;
                assert!((actual - expected).abs() < 1e-10);
                if column > 0 {
                    assert!(pitch.cents.abs() > 1.0);
                }
            }
        }
    }

    #[test]
    fn grid_uses_z_and_x_as_notes_and_stays_in_midi_range() {
        for layout in [Layout::Chromatic, Layout::SevenEdo] {
            assert_eq!(layout.octave_key(egui::Key::Z), None);
            assert_eq!(layout.octave_key(egui::Key::X), None);
            assert_eq!(layout.octave_key(egui::Key::PageUp), Some(true));
            for &(key, _) in ROWS.iter().flatten() {
                if let Some(pitch) = layout.pitch(key, layout.max_root()) {
                    assert!(pitch.note <= 127);
                }
            }
        }
    }
}
