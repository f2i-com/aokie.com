//! State-tensor bookkeeping shared across the flow LM and mimi decoder.
//!
//! ONNX's stateful-session convention: every `state_N` input has a matching
//! `out_state_N` output that carries the updated state forward. The bundle
//! describes each slot's dtype/shape/fill in `*_state_manifest`, and this
//! module handles the three boring-but-necessary pieces:
//!   1. initialising a fresh state buffer from the manifest,
//!   2. packing the state into an ORT input HashMap for a `session.run`,
//!   3. pulling the updated state back out of the session's outputs and
//!      writing it into the same buffer in place.

use std::collections::HashMap;

use ndarray::{ArrayD, IxDyn};
use ort::session::SessionOutputs;
use ort::value::{DynTensor, Tensor};

use super::{StateBuffers, StateSlot, StateValue};

/// Wrap each state slot as an ORT input keyed by its `input_name`.
pub fn state_inputs(
    state: &StateBuffers,
    manifest: &[StateSlot],
) -> Result<HashMap<String, DynTensor>, String> {
    let mut m = HashMap::with_capacity(manifest.len());
    for (slot, value) in manifest.iter().zip(state.slots.iter()) {
        let tensor: DynTensor = match value {
            StateValue::F32 { data, shape } => {
                let arr = ArrayD::from_shape_vec(IxDyn(shape), data.clone())
                    .map_err(|e| format!("{} shape: {}", slot.input_name, e))?;
                Tensor::from_array(arr)
                    .map_err(|e| format!("{} wrap f32: {}", slot.input_name, e))?
                    .upcast()
            }
            StateValue::I64 { data, shape } => {
                let arr = ArrayD::from_shape_vec(IxDyn(shape), data.clone())
                    .map_err(|e| format!("{} shape: {}", slot.input_name, e))?;
                Tensor::from_array(arr)
                    .map_err(|e| format!("{} wrap i64: {}", slot.input_name, e))?
                    .upcast()
            }
            StateValue::Bool { data, shape } => {
                let arr = ArrayD::from_shape_vec(IxDyn(shape), data.clone())
                    .map_err(|e| format!("{} shape: {}", slot.input_name, e))?;
                Tensor::from_array(arr)
                    .map_err(|e| format!("{} wrap bool: {}", slot.input_name, e))?
                    .upcast()
            }
        };
        m.insert(slot.input_name.clone(), tensor);
    }
    Ok(m)
}

/// Pull `out_state_N` tensors out of a session run and write them back into
/// `state` in place. `output_offset` is the number of non-state outputs that
/// come before the state block (2 for flow_lm_main: conditioning + eos_logit;
/// 1 for mimi_decoder: audio).
pub fn update_state_from_outputs(
    state: &mut StateBuffers,
    outputs: &SessionOutputs,
    manifest: &[StateSlot],
    output_offset: usize,
) -> Result<(), String> {
    // Outputs from ort are name-keyed. We look each `out_state_N` up directly
    // so the output order coming back doesn't have to match the manifest
    // order exactly.
    for (slot, value) in manifest.iter().zip(state.slots.iter_mut()) {
        let out = outputs.get(slot.output_name.as_str()).ok_or_else(|| {
            format!(
                "output '{}' missing (offset={})",
                slot.output_name, output_offset
            )
        })?;
        match value {
            StateValue::F32 { data, shape } => {
                let (out_shape, out_data) = out
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("extract {}: {}", slot.output_name, e))?;
                *shape = out_shape.iter().map(|&d| d as usize).collect();
                *data = out_data.to_vec();
            }
            StateValue::I64 { data, shape } => {
                let (out_shape, out_data) = out
                    .try_extract_tensor::<i64>()
                    .map_err(|e| format!("extract {}: {}", slot.output_name, e))?;
                *shape = out_shape.iter().map(|&d| d as usize).collect();
                *data = out_data.to_vec();
            }
            StateValue::Bool { data, shape } => {
                let (out_shape, out_data) = out
                    .try_extract_tensor::<bool>()
                    .map_err(|e| format!("extract {}: {}", slot.output_name, e))?;
                *shape = out_shape.iter().map(|&d| d as usize).collect();
                *data = out_data.to_vec();
            }
        }
    }
    Ok(())
}
