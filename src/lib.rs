#![deny(missing_docs)]

//! `deunoiser` is a crate for removing noise from audio. The main entry point is
//! [`DenoiseState`].
//!
//! [`DenoiseState`]: struct.DenoiseState.html

use once_cell::sync::OnceCell;

#[cfg(not(feature = "train"))]
mod util;

#[cfg(feature = "dasp")]
mod signal;
#[cfg(feature = "dasp")]
pub use dasp;

mod denoise;
mod features;
mod pitch;
mod rnn;

pub use denoise::DenoiseState;
pub use features::DenoiseFeatures;
pub use rnn::RnnModel;
#[cfg(feature = "dasp")]
pub use signal::DenoiseSignal;

#[doc(hidden)]
pub const FRAME_SIZE_SHIFT: usize = 2;
#[doc(hidden)]
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
#[doc(hidden)]
pub const FREQ_SIZE: usize = FRAME_SIZE + 1;

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

#[doc(hidden)]
pub const NB_BANDS: usize = 22;
pub(crate) const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
#[doc(hidden)]
pub const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
#[doc(hidden)]
pub const EBAND_5MS: [usize; 22] = [
  // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
  0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];
type Complex = easyfft::num_complex::Complex32;

/// Computes the correlation between two frequency-domain signals, and aggregates the correlation
/// into bands.
///
/// `out` is the output (duh), and it has length `NB_BANDS`.
pub(crate) fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
  for y in out.iter_mut() {
    *y = 0.0;
  }

  for i in 0..(NB_BANDS - 1) {
    let band_size: usize =
      (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
    for j in 0..band_size {
      let frac: f32 = j as f32 / band_size as f32;
      let idx: usize = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
      let corr: f32 = x[idx].re * p[idx].re + x[idx].im * p[idx].im;
      out[i] += (1.0 - frac) * corr;
      out[i + 1] += frac * corr;
    }
  }
  out[0] *= 2.0;
  out[NB_BANDS - 1] *= 2.0;
}

fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
  for y in out.iter_mut() {
    *y = 0.0;
  }

  for i in 0..(NB_BANDS - 1) {
    let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
    for j in 0..band_size {
      let frac = j as f32 / band_size as f32;
      let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
      out[idx] = (1.0 - frac) * band_e[i] + frac * band_e[i + 1];
    }
  }
}

struct CommonState {
  window: [f32; WINDOW_SIZE],
  dct_table: [f32; NB_BANDS * NB_BANDS],
  wnorm: f32,
}

static COMMON: OnceCell<CommonState> = OnceCell::new();

fn common() -> &'static CommonState {
  if COMMON.get().is_none() {
    let pi = std::f64::consts::PI;
    let mut window: [f32; 960] = [0.0; WINDOW_SIZE];
    for i in 0..FRAME_SIZE {
      let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
      window[i] = (0.5 * pi * sin * sin).sin() as f32;
      window[WINDOW_SIZE - i - 1] = (0.5 * pi * sin * sin).sin() as f32;
    }
    let wnorm: f32 = 1_f32 / window.iter().map(|x| x * x).sum::<f32>();

    let mut dct_table: [f32; 484] = [0.0; NB_BANDS * NB_BANDS];
    for i in 0..NB_BANDS {
      for j in 0..NB_BANDS {
        dct_table[i * NB_BANDS + j] =
          ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
        if j == 0 {
          dct_table[i * NB_BANDS + j] *= 0.5f32.sqrt();
        }
      }
    }

    let _ = COMMON.set(CommonState {
      window,
      dct_table,
      wnorm,
    });
  }
  COMMON.get().unwrap()
}

/// A brute-force DCT (discrete cosine transform) of size NB_BANDS.
pub(crate) fn dct(out: &mut [f32], x: &[f32]) {
  let c = common();
  for i in 0..NB_BANDS {
    let mut sum = 0.0;
    for j in 0..NB_BANDS {
      sum += x[j] * c.dct_table[j * NB_BANDS + i];
    }
    out[i] = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
  }
}

fn apply_window(output: &mut [f32], input: &[f32]) {
  let c: &CommonState = common();
  for (x, &y, &w) in util::zip3(output, input, &c.window[..]) {
    *x = y * w;
  }
}

fn apply_window_in_place(xs: &mut [f32]) {
  let c: &CommonState = common();
  for (x, &w) in xs.iter_mut().zip(&c.window[..]) {
    *x *= w;
  }
}
