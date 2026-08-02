//! Level, panorama, and explicit stereo-image utilities.

use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::kernel::{LinearSmoother, db_to_gain};
use crate::parameter::{
    ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanLaw {
    Linear,
    #[default]
    EqualPower,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Gain {
    pub enabled: bool,
    pub gain_db: f32,
    pub pan: f32,
    pub pan_law: PanLaw,
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    gain: LinearSmoother,
    #[serde(skip)]
    pan_smoother: LinearSmoother,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            enabled: true,
            gain_db: 0.0,
            pan: 0.0,
            pan_law: PanLaw::EqualPower,
            input_layout: None,
            maximum_block_size: 0,
            gain: LinearSmoother::default(),
            pan_smoother: LinearSmoother::default(),
        }
    }
}

const GAIN_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "gain_db",
        name: "Gain",
        kind: ParameterKind::Float {
            min: -120.0,
            max: 24.0,
        },
        unit: ParameterUnit::Decibels,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "pan",
        name: "Pan",
        kind: ParameterKind::Float {
            min: -1.0,
            max: 1.0,
        },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: Some("bipolar"),
    },
    ParameterDescriptor {
        id: "pan_law",
        name: "Pan Law",
        kind: ParameterKind::Choice(&["linear", "equal_power"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(1),
        automatable: false,
        display_hint: None,
    },
];

impl Processor for Gain {
    fn type_id(&self) -> &'static str {
        "gaw.gain"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(if input == AudioLayout::Mono {
            AudioLayout::Stereo
        } else {
            input
        })
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.gain = LinearSmoother::new(
            db_to_gain(self.gain_db.clamp(-120.0, 24.0)),
            spec.sample_rate,
            5.0,
        );
        self.pan_smoother = LinearSmoother::new(self.pan.clamp(-1.0, 1.0), spec.sample_rate, 5.0);
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _: ProcessContext,
    ) -> Result<(), ProcessError> {
        let layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let out_layout = self.output_layout(layout)?;
        let frames = validate_process_io(
            input,
            output,
            layout,
            out_layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let mut event = 0;
        for frame in 0..frames {
            while event < events.len() && events[event].sample_offset == frame {
                match events[event].id.as_str() {
                    "gain_db" => {
                        if let ParameterValue::Float(value) = events[event].value {
                            self.gain_db = value.clamp(-120.0, 24.0);
                            self.gain.set_target(db_to_gain(self.gain_db));
                        } else {
                            return Err(ProcessError::InvalidParameterValue(
                                events[event].id.clone(),
                            ));
                        }
                    }
                    "pan" => {
                        if let ParameterValue::Float(value) = events[event].value {
                            self.pan = value.clamp(-1.0, 1.0);
                            self.pan_smoother.set_target(self.pan);
                        } else {
                            return Err(ProcessError::InvalidParameterValue(
                                events[event].id.clone(),
                            ));
                        }
                    }
                    "pan_law" => {
                        if let ParameterValue::Choice(value) = events[event].value {
                            self.pan_law = if value == 0 {
                                PanLaw::Linear
                            } else {
                                PanLaw::EqualPower
                            };
                        } else {
                            return Err(ProcessError::InvalidParameterValue(
                                events[event].id.clone(),
                            ));
                        }
                    }
                    _ => return Err(ProcessError::UnknownParameter(events[event].id.clone())),
                }
                event += 1;
            }
            let gain = self.gain.next();
            let pan = self.pan_smoother.next();
            let (left, right) = match self.pan_law {
                PanLaw::Linear => ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0)),
                PanLaw::EqualPower => {
                    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                    (
                        angle.cos() * std::f32::consts::SQRT_2,
                        angle.sin() * std::f32::consts::SQRT_2,
                    )
                }
            };
            if input.len() == 1 {
                output[0][frame] = input[0][frame] * gain * left;
                output[1][frame] = input[0][frame] * gain * right;
            } else {
                output[0][frame] = input[0][frame] * gain * left;
                output[1][frame] = input[1][frame] * gain * right;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.gain.jump_to(db_to_gain(self.gain_db));
        self.pan_smoother.jump_to(self.pan);
    }
    fn seek(&mut self, _: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        GAIN_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StereoOutputLayout {
    #[default]
    Preserve,
    Mono,
    Stereo,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StereoTool {
    pub enabled: bool,
    pub balance: f32,
    pub width: f32,
    pub mid_gain_db: f32,
    pub side_gain_db: f32,
    pub swap_channels: bool,
    pub invert_left: bool,
    pub invert_right: bool,
    pub output_layout: StereoOutputLayout,
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for StereoTool {
    fn default() -> Self {
        Self {
            enabled: true,
            balance: 0.0,
            width: 1.0,
            mid_gain_db: 0.0,
            side_gain_db: 0.0,
            swap_channels: false,
            invert_left: false,
            invert_right: false,
            output_layout: StereoOutputLayout::Preserve,
            input_layout: None,
            maximum_block_size: 0,
        }
    }
}

const STEREO_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "balance",
        name: "Balance",
        kind: ParameterKind::Float {
            min: -1.0,
            max: 1.0,
        },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: Some("bipolar"),
    },
    ParameterDescriptor {
        id: "width",
        name: "Width",
        kind: ParameterKind::Float { min: 0.0, max: 2.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(1.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "mid_gain_db",
        name: "Mid Gain",
        kind: ParameterKind::Float {
            min: -120.0,
            max: 24.0,
        },
        unit: ParameterUnit::Decibels,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "side_gain_db",
        name: "Side Gain",
        kind: ParameterKind::Float {
            min: -120.0,
            max: 24.0,
        },
        unit: ParameterUnit::Decibels,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "swap_channels",
        name: "Swap",
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(false),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "invert_left",
        name: "Invert Left",
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(false),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "invert_right",
        name: "Invert Right",
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(false),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "output_layout",
        name: "Output Layout",
        kind: ParameterKind::Choice(&["preserve", "mono", "stereo"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
];

impl Processor for StereoTool {
    fn type_id(&self) -> &'static str {
        "gaw.stereo_tool"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(match self.output_layout {
            StereoOutputLayout::Preserve => input,
            StereoOutputLayout::Mono => AudioLayout::Mono,
            StereoOutputLayout::Stereo => AudioLayout::Stereo,
        })
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _: ProcessContext,
    ) -> Result<(), ProcessError> {
        let layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let out_layout = self.output_layout(layout)?;
        let frames = validate_process_io(
            input,
            output,
            layout,
            out_layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        for event in events {
            match (event.id.as_str(), event.value) {
                ("balance", ParameterValue::Float(v)) => self.balance = v.clamp(-1.0, 1.0),
                ("width", ParameterValue::Float(v)) => self.width = v.clamp(0.0, 2.0),
                ("mid_gain_db", ParameterValue::Float(v)) => {
                    self.mid_gain_db = v.clamp(-120.0, 24.0);
                }
                ("side_gain_db", ParameterValue::Float(v)) => {
                    self.side_gain_db = v.clamp(-120.0, 24.0);
                }
                ("swap_channels", ParameterValue::Bool(v)) => self.swap_channels = v,
                ("invert_left", ParameterValue::Bool(v)) => self.invert_left = v,
                ("invert_right", ParameterValue::Bool(v)) => self.invert_right = v,
                ("output_layout", ParameterValue::Choice(_)) => {
                    return Err(ProcessError::InvalidParameterValue(
                        "output_layout is not automatable".into(),
                    ));
                }
                (id, _) if STEREO_PARAMETERS.iter().any(|p| p.id == id) => {
                    return Err(ProcessError::InvalidParameterValue(event.id.clone()));
                }
                _ => return Err(ProcessError::UnknownParameter(event.id.clone())),
            }
        }
        let mid_gain = db_to_gain(self.mid_gain_db);
        let side_gain = db_to_gain(self.side_gain_db) * self.width;
        let balance = self.balance;
        for frame in 0..frames {
            let mut left = input[0][frame];
            let mut right = if input.len() == 2 {
                input[1][frame]
            } else {
                left
            };
            if self.swap_channels {
                std::mem::swap(&mut left, &mut right);
            }
            if self.invert_left {
                left = -left;
            }
            if self.invert_right {
                right = -right;
            }
            left *= if balance > 0.0 { 1.0 - balance } else { 1.0 };
            right *= if balance < 0.0 { 1.0 + balance } else { 1.0 };
            let mid = (left + right) * 0.5 * mid_gain;
            let side = (left - right) * 0.5 * side_gain;
            if output.len() == 1 {
                output[0][frame] = mid;
            } else {
                output[0][frame] = mid + side;
                output[1][frame] = mid - side;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {}
    fn seek(&mut self, _: u64) {}
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        STEREO_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stereo_downmix_is_explicit() {
        let mut tool = StereoTool {
            output_layout: StereoOutputLayout::Mono,
            ..Default::default()
        };
        tool.prepare(PrepareSpec::default()).unwrap();
        let left = [1.0, -1.0];
        let right = [-1.0, 1.0];
        let mut out = [9.0; 2];
        tool.process(
            &[&left, &right],
            &mut [&mut out],
            &[],
            ProcessContext::default(),
        )
        .unwrap();
        assert_eq!(out, [0.0, 0.0]);
    }
}
