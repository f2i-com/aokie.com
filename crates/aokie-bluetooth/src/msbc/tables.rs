//! SBC analysis / synthesis filter coefficients and LOUDNESS bit-allocation
//! tables, in the form needed by the 8-subband mSBC encoder/decoder.
//!
//! All values come from the A2DP SBC specification (Annex 12.B) and the
//! Bluedroid reference implementation. Only the 8-subband variants are
//! ported — mSBC is fixed to 8 subbands.

/// SBC analysis window `C[0..79]` for 8 subbands, in floating-point form.
///
/// These are the Bluedroid SBC encoder's `gas32CoeffFor8SBs` table values
/// (Q31 fixed-point) converted to `f32`. They differ from the values
/// published in earlier SBC references (which had a different sign
/// pattern that caused frequency-dependent gain in round-trip). Using
/// the Bluedroid values matches what Android phones produce on the wire
/// and gives a near-flat round-trip response.
#[rustfmt::skip]
pub const ANALYSIS_WINDOW_8: [f32; 80] = [
     0.00000000e+00,  1.56575348e-04,  3.43256164e-04,  5.54619823e-04,
     8.23919196e-04,  1.13992486e-03,  1.47640146e-03,  1.78371696e-03,
     2.01182533e-03,  2.10371986e-03,  1.99454511e-03,  1.61656272e-03,
     9.02154483e-04, -1.78805087e-04, -1.64973084e-03, -3.49717448e-03,
     5.65949455e-03,  8.02941155e-03,  1.04584442e-02,  1.27472333e-02,
     1.46525260e-02,  1.59045602e-02,  1.62208467e-02,  1.53184105e-02,
     1.29371802e-02,  8.85757525e-03,  2.92408420e-03, -4.91577992e-03,
    -1.46404072e-02, -2.61098752e-02, -3.90751380e-02, -5.31873028e-02,
     6.79989429e-02,  8.29847576e-02,  9.75753916e-02,  1.11196689e-01,
     1.23264548e-01,  1.33264415e-01,  1.40753505e-01,  1.45389847e-01,
     1.46955068e-01,  1.45389847e-01,  1.40753505e-01,  1.33264415e-01,
     1.23264548e-01,  1.11196689e-01,  9.75753916e-02,  8.29847576e-02,
    -6.79989429e-02, -5.31873028e-02, -3.90751380e-02, -2.61098752e-02,
    -1.46404072e-02, -4.91577992e-03,  2.92408420e-03,  8.85757525e-03,
     1.29371802e-02,  1.53184105e-02,  1.62208467e-02,  1.59045602e-02,
     1.46525260e-02,  1.27472333e-02,  1.04584442e-02,  8.02941155e-03,
    -5.65949455e-03, -3.49717448e-03, -1.64973084e-03, -1.78805087e-04,
     9.02154483e-04,  1.61656272e-03,  1.99454511e-03,  2.10371986e-03,
     2.01182533e-03,  1.78371696e-03,  1.47640146e-03,  1.13992486e-03,
     8.23919196e-04,  5.54619823e-04,  3.43256164e-04,  1.56575348e-04,
];

/// SBC synthesis window `D[0..79]` for 8 subbands. The standard SBC
/// relation for 8-subband decoding is `D[i] = 8 * C[i]` (the analysis
/// window scaled by the number of subbands). We compute it at runtime
/// once via `synthesis_window_8()` rather than storing duplicate data.
pub fn synthesis_window_8() -> [f32; 80] {
    let mut d = [0.0f32; 80];
    for i in 0..80 {
        d[i] = 8.0 * ANALYSIS_WINDOW_8[i];
    }
    d
}

/// 16×8 inverse DCT (synthesis) cosine matrix `N`.
///
/// `N[i][k] = cos((k + 0.5)(i + 4) π / 8)` for `i ∈ 0..15`, `k ∈ 0..7`,
/// matching the Bluedroid SBC reference (`N[row][col]` where row is the
/// 16-output index and col is the 8-subband index). Pairs with the
/// analysis matrix `cos((i + 0.5)(k - 4) π / 8)`.
pub fn synthesis_cosine_matrix() -> [[f32; 8]; 16] {
    let mut n = [[0.0f32; 8]; 16];
    let pi_over_8 = std::f32::consts::PI / 8.0;
    for i in 0..16 {
        for k in 0..8 {
            let arg = (k as f32 + 0.5) * (i as f32 + 4.0) * pi_over_8;
            n[i][k] = arg.cos();
        }
    }
    n
}

/// 8×16 forward DCT (analysis) cosine matrix `M`.
///
/// `M[i][k] = cos((i + 0.5) * (k - 4) * π / 8)` for
/// `i ∈ 0..7`, `k ∈ 0..15`. (Only `k ∈ 0..15` is used per the polyphase
/// reduction; values for `k ≥ 8` repeat with sign.)
pub fn analysis_cosine_matrix() -> [[f32; 16]; 8] {
    let mut m = [[0.0f32; 16]; 8];
    let pi_over_8 = std::f32::consts::PI / 8.0;
    for i in 0..8 {
        for k in 0..16 {
            let arg = (i as f32 + 0.5) * (k as f32 - 4.0) * pi_over_8;
            m[i][k] = arg.cos();
        }
    }
    m
}

/// LOUDNESS bit-allocation offsets for 8 subbands, indexed
/// `[sample_freq][subband]`. Sample-rate index follows the SBC spec:
/// 0 = 16 kHz, 1 = 32 kHz, 2 = 44.1 kHz, 3 = 48 kHz. Only row 0 is used
/// by mSBC, but the rest are kept verbatim for completeness.
#[rustfmt::skip]
pub const LOUDNESS_OFFSETS_8: [[i32; 8]; 4] = [
    // 16 kHz
    [-2,  0,  0,  0,  0,  0,  0,  1],
    // 32 kHz
    [-3,  0,  0,  0,  0,  0,  1,  2],
    // 44.1 kHz
    [-4,  0,  0,  0,  0,  0,  1,  2],
    // 48 kHz
    [-4,  0,  0,  0,  0,  0,  1,  2],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_window_has_full_length() {
        assert_eq!(ANALYSIS_WINDOW_8.len(), 80);
    }

    #[test]
    fn synthesis_window_has_full_length() {
        assert_eq!(synthesis_window_8().len(), 80);
    }

    #[test]
    fn synthesis_window_is_eight_times_analysis() {
        let d = synthesis_window_8();
        for i in 0..80 {
            let expected = 8.0 * ANALYSIS_WINDOW_8[i];
            assert!((d[i] - expected).abs() < 1e-6, "D[{}] mismatch", i);
        }
    }

    #[test]
    fn loudness_table_has_one_row_per_sample_rate() {
        assert_eq!(LOUDNESS_OFFSETS_8.len(), 4);
        for row in LOUDNESS_OFFSETS_8 {
            assert_eq!(row.len(), 8);
        }
    }

    #[test]
    fn synthesis_cosine_matrix_shape() {
        let n = synthesis_cosine_matrix();
        // Spot-check: N[0][0] = cos(0.5 * 4 * π/8) = cos(π/4) = √2/2
        let expected = (std::f32::consts::PI / 4.0).cos();
        assert!((n[0][0] - expected).abs() < 1e-6);
    }

    #[test]
    fn analysis_cosine_matrix_shape() {
        let m = analysis_cosine_matrix();
        // M[0][4] = cos(0.5 * 0 * π/8) = 1
        assert!((m[0][4] - 1.0).abs() < 1e-6);
        // M[0][0] = cos(0.5 * -4 * π/8) = cos(-π/4) = √2/2
        let expected = (-2.0_f32 * std::f32::consts::PI / 8.0).cos();
        assert!((m[0][0] - expected).abs() < 1e-6);
    }
}
