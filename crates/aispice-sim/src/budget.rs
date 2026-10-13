//! Spec budgets: a chip-level target split into the specs each block must
//! meet, with first-order equations a designer can follow and override.
//!
//! Every step of a budget is kept with its equation and its numbers, and the
//! block specs come out in the measurement names `check_specs` evaluates, so
//! a block template (the two-stage op-amp, the LDO) can be checked against
//! them directly. The system model is the equation set itself, in Rust: it
//! needs nothing installed.

use aispice_core::units;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::f64::consts::{LN_2, PI};

/// Boltzmann's constant, J/K.
const K_B: f64 = 1.380_649e-23;

/// One step of a derivation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub quantity: String,
    /// The equation, in plain text.
    pub equation: String,
    pub value: f64,
    pub unit: String,
}

/// The spec table one block must meet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockSpecs {
    /// The block, also the suggested file stem: `ota`, `error_amp`.
    pub block: String,
    /// The test bench the measurements assume, written into the file.
    pub bench: String,
    /// Spec lines, as `check_specs` reads them.
    pub specs: Vec<String>,
}

impl BlockSpecs {
    /// The `.specs` file text.
    pub fn file_text(&self, budget: &str) -> String {
        let mut s = format!(
            "# Specs for the {} from the aispice {budget} budget.\n# Test bench: {}\n",
            self.block, self.bench
        );
        for line in &self.specs {
            s.push_str(line);
            s.push('\n');
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Budget {
    /// `adc_frontend` or `ldo`.
    pub kind: String,
    pub steps: Vec<Step>,
    pub blocks: Vec<BlockSpecs>,
    /// Assumptions a designer should check.
    pub notes: Vec<String>,
}

impl Budget {
    /// The derivation as text, one step a line.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for st in &self.steps {
            s.push_str(&format!(
                "  {}: {} = {}\n",
                st.quantity,
                st.equation,
                units::format_with_unit(st.value, &st.unit)
            ));
        }
        for n in &self.notes {
            s.push_str(&format!("  note: {n}\n"));
        }
        s
    }
}

/// A budget the inputs cannot meet.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct BudgetError(pub String);

fn positive(name: &str, v: f64) -> Result<f64, BudgetError> {
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(BudgetError(format!("{name} must be a positive number")))
    }
}

fn fraction(name: &str, v: f64) -> Result<f64, BudgetError> {
    if v.is_finite() && v > 0.0 && v < 1.0 {
        Ok(v)
    } else {
        Err(BudgetError(format!("{name} must be between 0 and 1")))
    }
}

fn kelvin(celsius: f64) -> f64 {
    celsius + 273.15
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// A spec value written as `check_specs` reads it, rounded outward to three
/// significant digits so the spec is never looser than the derivation.
fn limit(v: f64, up: bool) -> String {
    if v == 0.0 || !v.is_finite() {
        return units::format(v);
    }
    let mag = 10f64.powf(v.abs().log10().floor() - 2.0);
    let r = if up {
        (v / mag).ceil() * mag
    } else {
        (v / mag).floor() * mag
    };
    units::format(r)
}

/// The sample-and-hold or residue stage of an ADC: a switched-capacitor
/// circuit around an OTA in a closed loop with feedback factor `beta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdcFrontEnd {
    /// Resolution in bits.
    pub bits: u32,
    /// Effective number of bits the stage must keep. Give this or `snr_db`.
    #[serde(default)]
    pub enob: Option<f64>,
    /// Signal-to-noise ratio in dB, instead of `enob`.
    #[serde(default)]
    pub snr_db: Option<f64>,
    /// Differential full scale, peak to peak, in volts.
    pub full_scale: f64,
    /// Sample rate in samples per second.
    pub sample_rate: f64,
    /// Closed-loop feedback factor of the stage (0.5 for a unity-gain
    /// sample and hold with equal capacitors).
    #[serde(default = "half")]
    pub beta: f64,
    /// Share of the thermal noise given to the sampling capacitors' kT/C;
    /// the OTA gets the rest. Default 0.5.
    #[serde(default = "half")]
    pub ktc_share: f64,
    /// Share of the half clock period left for linear settling. Default 0.6.
    #[serde(default = "settle_default")]
    pub settle_share: f64,
    /// Share of the half clock period for slewing. Default 0.2.
    #[serde(default = "slew_default")]
    pub slew_share: f64,
    /// The largest output step the OTA slews, in volts (default: the full
    /// scale).
    #[serde(default)]
    pub step: Option<f64>,
    /// Temperature in Celsius for kT (default 27).
    #[serde(default = "room")]
    pub temperature: f64,
}

fn half() -> f64 {
    0.5
}
fn settle_default() -> f64 {
    0.6
}
fn slew_default() -> f64 {
    0.2
}
fn room() -> f64 {
    27.0
}

/// Budget an ADC front end: SNR to noise power, kT/C capacitor, OTA input
/// noise, DC gain, gain-bandwidth, slew rate and phase margin.
pub fn adc_frontend(p: &AdcFrontEnd) -> Result<Budget, BudgetError> {
    if !(1..=24).contains(&p.bits) {
        return Err(BudgetError("bits must be from 1 to 24".into()));
    }
    let vfs = positive("full_scale", p.full_scale)?;
    let fs = positive("sample_rate", p.sample_rate)?;
    let beta = fraction("beta", p.beta)?;
    let alpha = fraction("ktc_share", p.ktc_share)?;
    let settle = fraction("settle_share", p.settle_share)?;
    let slew = fraction("slew_share", p.slew_share)?;
    if settle + slew >= 1.0 {
        return Err(BudgetError(
            "settle_share and slew_share together must leave part of the half period".into(),
        ));
    }
    let t = kelvin(p.temperature);
    let n = p.bits as f64;
    let mut steps = Vec::new();
    let mut step = |quantity: &str, equation: String, value: f64, unit: &str| {
        steps.push(Step {
            quantity: quantity.into(),
            equation,
            value,
            unit: unit.into(),
        });
        value
    };
    let snr = match (p.enob, p.snr_db) {
        (Some(enob), None) => {
            if !(enob > 0.0 && enob <= n) {
                return Err(BudgetError(format!(
                    "enob must be above 0 and at most bits ({n})"
                )));
            }
            step(
                "SNR",
                format!("6.02 ENOB + 1.76 dB with ENOB = {enob}"),
                6.02 * enob + 1.76,
                "dB",
            )
        }
        (None, Some(snr)) => step("SNR", "given".into(), positive("snr_db", snr)?, "dB"),
        _ => return Err(BudgetError("give enob or snr_db, not both".into())),
    };
    let p_signal = vfs * vfs / 8.0;
    let p_noise = step(
        "total noise power",
        format!(
            "(Vfs^2 / 8) / 10^(SNR / 10) with Vfs = {}",
            units::format_with_unit(vfs, "V")
        ),
        p_signal / 10f64.powf(snr / 10.0),
        "V^2",
    );
    let lsb = vfs / 2f64.powi(p.bits as i32);
    let p_q = step(
        "quantization noise power",
        format!(
            "LSB^2 / 12 with LSB = Vfs / 2^{} = {}",
            p.bits,
            units::format_with_unit(lsb, "V")
        ),
        lsb * lsb / 12.0,
        "V^2",
    );
    let p_th = p_noise - p_q;
    // An ENOB at the resolution leaves the circuit nothing (6.02 dB a bit
    // is rounded, so not exactly nothing); under 1% would ask for an
    // absurd capacitor.
    if p_th <= 0.01 * p_noise {
        return Err(BudgetError(format!(
            "an SNR of {snr:.1} dB leaves almost no noise for the circuit after quantization; ask for fewer effective bits than {n}"
        )));
    }
    step(
        "thermal noise budget",
        "total minus quantization".into(),
        p_th,
        "V^2",
    );
    let c = step(
        "sampling capacitor (each side)",
        format!(
            "2 k T / (a P_th), differential, with a = {alpha} of the thermal budget, T = {t:.1} K"
        ),
        2.0 * K_B * t / (alpha * p_th),
        "F",
    );
    let half_period = 1.0 / (2.0 * fs);
    let t_settle = settle * half_period;
    let gbw = step(
        "OTA gain-bandwidth",
        format!(
            "(N + 1) ln 2 / (2 pi beta t_settle): settling to half an LSB in t_settle = {} of 1 / (2 fs) = {}",
            settle,
            units::format_with_unit(t_settle, "s")
        ),
        (n + 1.0) * LN_2 / (2.0 * PI * beta * t_settle),
        "Hz",
    );
    let a0 = step(
        "OTA DC gain",
        format!("2^(N + 1) / beta: static error below half an LSB with beta = {beta}"),
        2f64.powf(n + 1.0) / beta,
        "",
    );
    let gain_db = step("OTA DC gain in dB", "20 log10(A0)".into(), db(a0), "dB");
    let en = step(
        "OTA input noise density",
        "sqrt((1 - a) P_th / ((pi / 2) beta GBW)): the rest of the budget over the loop's noise bandwidth"
            .to_string(),
        ((1.0 - alpha) * p_th / (PI / 2.0 * beta * gbw)).sqrt(),
        "V/rtHz",
    );
    let v_step = p.step.unwrap_or(vfs);
    let sr = step(
        "OTA slew rate",
        format!(
            "Vstep / t_slew with Vstep = {} and t_slew = {} of the half period",
            units::format_with_unit(v_step, "V"),
            slew
        ),
        v_step / (slew * half_period),
        "V/s",
    );
    let pm = step(
        "OTA phase margin",
        "single-pole settling".into(),
        60.0,
        "deg",
    );
    // Density measured where the OTA's white noise floor is, well above its
    // flicker corner and below the loop bandwidth.
    let f_noise = (gbw / 100.0).clamp(10e3, 10e6);
    let ota = BlockSpecs {
        block: "ota".into(),
        bench: format!(
            "the OTA open loop (an .ac run with its output V(out) and its input carrying AC 1, as the two_stage template) plus a .noise run referred to that input; the sampling capacitor is {}",
            units::format_with_unit(c, "F")
        ),
        specs: vec![
            format!("gain = gain_db_at(V(out), 10) >= {}", limit(gain_db, true)),
            format!("gbw = unity_gain_freq(V(out)) >= {}", limit(gbw, true)),
            format!("pm = phase_margin(V(out)) >= {}", limit(pm, true)),
            format!(
                "vn = input_noise_at({}) <= {}",
                units::format(f_noise),
                limit(en, false)
            ),
        ],
    };
    Ok(Budget {
        kind: "adc_frontend".into(),
        steps,
        blocks: vec![ota],
        notes: vec![
            format!(
                "slew rate {} with the load the stage drives sets the OTA's output current: I >= SR (C_load + C_s)",
                units::format_with_unit(sr, "V/s")
            ),
            "the equations are first order: single-pole settling, no incomplete-settling or nonlinear-gain error; leave margin and simulate".into(),
        ],
    })
}

/// A low-dropout regulator with a resistive divider from its output to the
/// error amplifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ldo {
    /// Lowest input voltage.
    pub vin_min: f64,
    /// Output voltage.
    pub vout: f64,
    /// Reference voltage the divider compares the output with.
    pub vref: f64,
    /// Largest load current, amps.
    pub iload_max: f64,
    /// Allowed output change from no load to full load, volts.
    pub load_regulation: f64,
    /// Supply rejection at DC, dB.
    pub psrr_dc_db: f64,
    /// Supply rejection at `psrr_freq`, dB.
    pub psrr_db: f64,
    pub psrr_freq: f64,
    /// Integrated output noise allowed, volts rms.
    pub noise_rms: f64,
    /// Quiescent current budget, amps.
    pub iq: f64,
    /// Overdrive of the pass device at full load (default 0.2 V).
    #[serde(default = "overdrive")]
    pub overdrive: f64,
    /// Share of the quiescent current the divider takes (default 0.1).
    #[serde(default = "divider_share")]
    pub divider_share: f64,
}

fn overdrive() -> f64 {
    0.2
}
fn divider_share() -> f64 {
    0.1
}

/// Budget an LDO: divider, pass device, loop gain from load regulation and
/// PSRR, loop bandwidth, error amplifier gain and bandwidth, and noise.
pub fn ldo(p: &Ldo) -> Result<Budget, BudgetError> {
    let vin = positive("vin_min", p.vin_min)?;
    let vout = positive("vout", p.vout)?;
    let vref = positive("vref", p.vref)?;
    let imax = positive("iload_max", p.iload_max)?;
    let reg = positive("load_regulation", p.load_regulation)?;
    let psrr_dc = positive("psrr_dc_db", p.psrr_dc_db)?;
    let psrr_f = positive("psrr_db", p.psrr_db)?;
    let f = positive("psrr_freq", p.psrr_freq)?;
    let noise = positive("noise_rms", p.noise_rms)?;
    let iq = positive("iq", p.iq)?;
    let vov = positive("overdrive", p.overdrive)?;
    let share = fraction("divider_share", p.divider_share)?;
    if vref > vout {
        return Err(BudgetError(
            "vref must not be above vout: the divider only divides".into(),
        ));
    }
    if vin <= vout {
        return Err(BudgetError("vin_min must be above vout".into()));
    }
    let mut steps = Vec::new();
    let mut step = |quantity: &str, equation: String, value: f64, unit: &str| {
        steps.push(Step {
            quantity: quantity.into(),
            equation,
            value,
            unit: unit.into(),
        });
        value
    };
    let beta = step("feedback factor", "Vref / Vout".into(), vref / vout, "");
    let i_div = share * iq;
    let r2 = step(
        "divider, lower resistor",
        format!(
            "Vref / I_div with I_div = {share} of Iq = {}",
            units::format_with_unit(i_div, "A")
        ),
        vref / i_div,
        "Ohm",
    );
    step(
        "divider, upper resistor",
        "R2 (1 / beta - 1)".into(),
        r2 * (1.0 / beta - 1.0),
        "Ohm",
    );
    step(
        "pass device on-resistance",
        "(Vin_min - Vout) / Iload_max: the dropout at full load".into(),
        (vin - vout) / imax,
        "Ohm",
    );
    let gm = step(
        "pass device transconductance at full load",
        format!(
            "2 Iload_max / Vov with Vov = {}",
            units::format_with_unit(vov, "V")
        ),
        2.0 * imax / vov,
        "S",
    );
    let t_reg = step(
        "loop gain for load regulation",
        format!(
            "(Iload_max / gm_pass) / dVout: the open-loop output change over the allowed {}",
            units::format_with_unit(reg, "V")
        ),
        imax / gm / reg,
        "",
    );
    let t_psrr = step(
        "loop gain for DC PSRR",
        "10^(PSRR_dc / 20)".into(),
        10f64.powf(psrr_dc / 20.0),
        "",
    );
    let t0 = step(
        "DC loop gain",
        "the larger of the two".into(),
        t_reg.max(t_psrr),
        "",
    );
    let a_pass = step(
        "pass stage gain at full load",
        "gm_pass Vout / Iload_max = 2 Vout / Vov".into(),
        2.0 * vout / vov,
        "",
    );
    let a_ea = step(
        "error amplifier DC gain",
        "T0 / (beta A_pass)".into(),
        t0 / (beta * a_pass),
        "",
    );
    let a_ea_db = step(
        "error amplifier DC gain in dB",
        "20 log10(A_ea)".into(),
        db(a_ea),
        "dB",
    );
    let fu = step(
        "loop unity-gain frequency",
        format!(
            "f 10^(PSRR_f / 20) at f = {}: a single-pole loop keeps that much gain there",
            units::format_with_unit(f, "Hz")
        ),
        f * 10f64.powf(psrr_f / 20.0),
        "Hz",
    );
    let gbw_ea = step(
        "error amplifier gain-bandwidth",
        "f_u / (beta A_pass)".into(),
        fu / (beta * a_pass),
        "Hz",
    );
    let vn_in = step(
        "input-referred noise (reference and error amplifier)",
        "beta Vn_out: the loop amplifies input noise by 1 / beta".into(),
        beta * noise,
        "Vrms",
    );
    let ea = BlockSpecs {
        block: "error_amp".into(),
        bench: "the error amplifier open loop (an .ac run with its output V(out) and its input carrying AC 1)".into(),
        specs: vec![
            format!("gain = gain_db_at(V(out), 10) >= {}", limit(a_ea_db, true)),
            format!("gbw = unity_gain_freq(V(out)) >= {}", limit(gbw_ea, true)),
            "pm = phase_margin(V(out)) >= 60".into(),
        ],
    };
    let closed = BlockSpecs {
        block: "ldo".into(),
        bench: format!(
            "the regulator closed loop, an .ac run with AC 1 on the reference and the supply source named VIN, and a .noise run of V(out); output at {}",
            units::format_with_unit(vout, "V")
        ),
        specs: vec![
            format!(
                "psrr_dc = psrr_db_at(V(out), VIN, 10) >= {}",
                limit(psrr_dc, true)
            ),
            format!(
                "psrr = psrr_db_at(V(out), VIN, {}) >= {}",
                units::format(f),
                limit(psrr_f, true)
            ),
            format!(
                "noise = output_noise_rms(10, 100k) <= {}",
                limit(noise, false)
            ),
        ],
    };
    Ok(Budget {
        kind: "ldo".into(),
        steps,
        blocks: vec![ea, closed],
        notes: vec![
            format!(
                "the reference and the error amplifier share {} rms of input-referred noise",
                units::format_with_unit(vn_in, "V")
            ),
            "the pass stage gain falls at light load and the output pole moves with the load: check stability at both ends and simulate".into(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(b: &Budget, q: &str) -> f64 {
        b.steps
            .iter()
            .find(|s| s.quantity == q)
            .unwrap_or_else(|| panic!("no {q}"))
            .value
    }

    fn rel(a: f64, b: f64) -> f64 {
        ((a - b) / b).abs()
    }

    /// A 10-bit stage at 9.5 effective bits, 1 V peak to peak, 50 MS/s,
    /// worked by hand.
    #[test]
    fn adc_frontend_matches_the_hand_calculation() {
        let p = AdcFrontEnd {
            bits: 10,
            enob: Some(9.5),
            snr_db: None,
            full_scale: 1.0,
            sample_rate: 50e6,
            beta: 0.5,
            ktc_share: 0.5,
            settle_share: 0.6,
            slew_share: 0.2,
            step: None,
            temperature: 27.0,
        };
        let b = adc_frontend(&p).unwrap();
        // SNR 58.95 dB; noise (1/8) / 10^5.895 = 1.591e-7 V^2.
        assert!((value(&b, "SNR") - 58.95).abs() < 1e-9);
        let pn = 0.125 / 10f64.powf(5.895);
        assert!(rel(value(&b, "total noise power"), pn) < 1e-9);
        // LSB 0.977 mV, quantization 7.947e-8 V^2.
        let pq = (1.0 / 1024.0f64).powi(2) / 12.0;
        let pth = pn - pq;
        assert!(rel(value(&b, "thermal noise budget"), pth) < 1e-9);
        // C = 2 k 300.15 / (0.5 pth) = 0.208 pF.
        let c = 2.0 * 1.380649e-23 * 300.15 / (0.5 * pth);
        assert!(rel(value(&b, "sampling capacitor (each side)"), c) < 1e-9);
        assert!((0.2e-12..0.22e-12).contains(&c), "{c}");
        // t_settle = 0.6 * 10 ns = 6 ns; GBW = 11 ln2 / (2 pi 0.5 6n) = 404.5 MHz.
        let gbw = 11.0 * LN_2 / (2.0 * PI * 0.5 * 6e-9);
        assert!(rel(value(&b, "OTA gain-bandwidth"), gbw) < 1e-9);
        assert!((404e6..405e6).contains(&gbw), "{gbw}");
        // A0 = 2^11 / 0.5 = 4096, 72.2 dB.
        assert_eq!(value(&b, "OTA DC gain"), 4096.0);
        assert!((value(&b, "OTA DC gain in dB") - 72.25).abs() < 0.01);
        // SR = 1 V / (0.2 * 10 ns) = 500 V/us.
        assert!(rel(value(&b, "OTA slew rate"), 5e8) < 1e-9);
        let en = (0.5 * pth / (PI / 2.0 * 0.5 * gbw)).sqrt();
        assert!(rel(value(&b, "OTA input noise density"), en) < 1e-9);
        // The spec file reads back with check_specs' parser, limits rounded
        // outward.
        let text = b.blocks[0].file_text("adc_frontend");
        let specs = crate::spec::parse_specs(&text).unwrap();
        assert_eq!(specs.len(), 4);
        assert!(specs[0].min.unwrap() >= 72.25);
        assert!(specs[1].min.unwrap() >= gbw);
        assert!(specs[3].max.unwrap() <= en);
    }

    #[test]
    fn adc_frontend_refuses_impossible_targets() {
        let mut p = AdcFrontEnd {
            bits: 8,
            enob: Some(8.0),
            snr_db: None,
            full_scale: 1.0,
            sample_rate: 1e6,
            beta: 0.5,
            ktc_share: 0.5,
            settle_share: 0.6,
            slew_share: 0.2,
            step: None,
            temperature: 27.0,
        };
        assert!(
            adc_frontend(&p)
                .unwrap_err()
                .0
                .contains("leaves almost no noise")
        );
        p.enob = None;
        assert!(
            adc_frontend(&p)
                .unwrap_err()
                .0
                .contains("give enob or snr_db")
        );
        p.snr_db = Some(40.0);
        p.settle_share = 0.9;
        assert!(adc_frontend(&p).unwrap_err().0.contains("half period"));
    }

    /// 3.3 V to 1.2 V at 10 mA, worked by hand.
    #[test]
    fn ldo_matches_the_hand_calculation() {
        let p = Ldo {
            vin_min: 2.7,
            vout: 1.2,
            vref: 0.6,
            iload_max: 10e-3,
            load_regulation: 10e-3,
            psrr_dc_db: 40.0,
            psrr_db: 20.0,
            psrr_freq: 100e3,
            noise_rms: 100e-6,
            iq: 50e-6,
            overdrive: 0.2,
            divider_share: 0.1,
        };
        let b = ldo(&p).unwrap();
        assert_eq!(value(&b, "feedback factor"), 0.5);
        // I_div 5 uA: R2 = 120 k, R1 = 120 k.
        assert!(rel(value(&b, "divider, lower resistor"), 120e3) < 1e-9);
        assert!(rel(value(&b, "divider, upper resistor"), 120e3) < 1e-9);
        assert!(rel(value(&b, "pass device on-resistance"), 150.0) < 1e-9);
        // gm 0.1 S; open-loop change 10 mA / 0.1 S = 0.1 V, over 10 mV: T = 10.
        assert!(rel(value(&b, "loop gain for load regulation"), 10.0) < 1e-9);
        // DC PSRR 40 dB needs T = 100, which wins.
        assert!(rel(value(&b, "DC loop gain"), 100.0) < 1e-9);
        // A_pass = 12; A_ea = 100 / (0.5 12) = 16.7.
        assert!(rel(value(&b, "error amplifier DC gain"), 100.0 / 6.0) < 1e-9);
        // f_u = 100 kHz * 10 = 1 MHz; GBW_ea = 1 MHz / 6.
        assert!(rel(value(&b, "loop unity-gain frequency"), 1e6) < 1e-9);
        assert!(rel(value(&b, "error amplifier gain-bandwidth"), 1e6 / 6.0) < 1e-9);
        assert!(
            rel(
                value(&b, "input-referred noise (reference and error amplifier)"),
                50e-6
            ) < 1e-9
        );
        for block in &b.blocks {
            crate::spec::parse_specs(&block.file_text("ldo")).unwrap();
        }
        let mut bad = p.clone();
        bad.vref = 1.5;
        assert!(ldo(&bad).is_err());
    }

    #[test]
    fn limits_round_outward() {
        assert_eq!(limit(72.247, true), "72.3");
        assert_eq!(limit(404_456_789.0, true), "405Meg");
        assert_eq!(limit(1.2345e-8, false), "12.3n");
    }
}
