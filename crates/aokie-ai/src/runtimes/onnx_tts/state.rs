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
use ort::session::{SessionInputValue, SessionOutputs};
use ort::value::{DynTensor, DynValue, Tensor};

use super::{StateBuffers, StateSlot, StateValue};

/// Build one ORT tensor from a host-side state slot (a fresh copy of the
/// host data — callers on the hot path should use [`LiveState`] instead,
/// which only pays this once per utterance).
fn slot_tensor(slot: &StateSlot, value: &StateValue) -> Result<DynTensor, String> {
    Ok(match value {
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
    })
}

/// Wrap each state slot as an ORT input keyed by its `input_name`.
pub fn state_inputs(
    state: &StateBuffers,
    manifest: &[StateSlot],
) -> Result<HashMap<String, DynTensor>, String> {
    let mut m = HashMap::with_capacity(manifest.len());
    for (slot, value) in manifest.iter().zip(state.slots.iter()) {
        m.insert(slot.input_name.clone(), slot_tensor(slot, value)?);
    }
    Ok(m)
}

/// Per-utterance state held as LIVE ort values instead of host `Vec`s.
///
/// The old flow round-tripped every state tensor through host memory on
/// every 80 ms frame (`try_extract_tensor().to_vec()` out, `data.clone()`
/// + `Tensor::from_array` back in) — with the flow LM's ~8 MB-per-layer KV
/// caches that was >100 MB of memcpy per generated frame. Here each run's
/// `out_state_N` output VALUE becomes the next run's `state_N` input
/// directly: on the CPU EP that is zero-copy, and on CUDA only ORT's own
/// transfer nodes touch the data. Host copies happen exactly once, at
/// `from_buffers`.
pub struct LiveState {
    slots: Vec<Option<DynValue>>,
}

impl LiveState {
    /// One-time host → ORT-value copy of a (cached) state buffer set.
    pub fn from_buffers(state: &StateBuffers, manifest: &[StateSlot]) -> Result<Self, String> {
        if state.slots.len() != manifest.len() {
            return Err(format!(
                "state has {} slots but manifest describes {}",
                state.slots.len(),
                manifest.len()
            ));
        }
        let slots = manifest
            .iter()
            .zip(state.slots.iter())
            .map(|(slot, value)| slot_tensor(slot, value).map(|t| Some(t.into_dyn())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { slots })
    }

    /// Move every slot out as an owned session input keyed by `state_N`.
    /// The slots are left empty; `absorb_outputs` MUST refill them from the
    /// run's `out_state_N` values before the next call (a failed run aborts
    /// the whole synthesis, so a half-consumed state is never reused).
    pub fn take_inputs(
        &mut self,
        manifest: &[StateSlot],
        extra_capacity: usize,
    ) -> Result<HashMap<String, SessionInputValue<'static>>, String> {
        let mut m = HashMap::with_capacity(manifest.len() + extra_capacity);
        for (slot, value) in manifest.iter().zip(self.slots.iter_mut()) {
            let v = value
                .take()
                .ok_or_else(|| format!("state slot {} consumed twice", slot.input_name))?;
            m.insert(slot.input_name.clone(), SessionInputValue::Owned(v));
        }
        Ok(m)
    }

    /// Take each `out_state_N` value out of the run's outputs and store it
    /// as the next run's `state_N` input.
    pub fn absorb_outputs(
        &mut self,
        outputs: &mut SessionOutputs,
        manifest: &[StateSlot],
    ) -> Result<(), String> {
        for (slot, value) in manifest.iter().zip(self.slots.iter_mut()) {
            let v = outputs
                .remove(slot.output_name.as_str())
                .ok_or_else(|| format!("output '{}' missing", slot.output_name))?;
            *value = Some(v);
        }
        Ok(())
    }

    /// Bind every slot's live value as a session input (the CUDA IoBinding
    /// path). Values are NOT consumed — the binding holds its own reference;
    /// `absorb_outputs` replaces them after the run as usual.
    pub fn bind_inputs(
        &self,
        binding: &mut ort::session::IoBinding,
        manifest: &[StateSlot],
    ) -> Result<(), String> {
        for (slot, value) in manifest.iter().zip(self.slots.iter()) {
            let v = value
                .as_ref()
                .ok_or_else(|| format!("state slot {} is empty", slot.input_name))?;
            binding
                .bind_input(slot.input_name.as_str(), v)
                .map_err(|e| format!("bind {}: {e}", slot.input_name))?;
        }
        Ok(())
    }
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
