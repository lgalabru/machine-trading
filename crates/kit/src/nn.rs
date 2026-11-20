//! Weights for a small neural network, embedded in the strategy.
//!
//! Train offline, save the weights as
//! [safetensors](https://github.com/huggingface/safetensors) (from candle
//! with `candle::safetensors::save`, or from PyTorch with
//! `safetensors.torch.save_file`), and embed the file:
//!
//! ```
//! use mtx_kit::{
//!     candle::{self, Module, Tensor},
//!     candle_nn, nn,
//! };
//!
//! /// `weights` is the embedded file:
//! /// `include_bytes!("../model.safetensors")`.
//! fn predict(weights: &[u8], features: [f32; 8]) -> candle::Result<Tensor> {
//!     let weights = nn::var_builder(weights)?;
//!     // Build the layers once, in `start`.
//!     let hidden = candle_nn::linear(8, 32, weights.pp("hidden"))?;
//!     let out = candle_nn::linear(32, 1, weights.pp("out"))?;
//!     // Every step: one row of features in, one prediction out.
//!     let features = Tensor::new(&[features], &nn::DEVICE)?;
//!     out.forward(&hidden.forward(&features)?.relu()?)
//! }
//! # assert!(predict(b"not safetensors", [0.0; 8]).is_err());
//! ```
//!
//! `candle_nn::linear(.., weights.pp("hidden"))` reads the tensors
//! `hidden.weight` (shape `[out, in]`, as PyTorch stores it) and
//! `hidden.bias`. A name the file lacks, or a shape that differs, is an
//! error when the layer is built: build the layers in `start`, and return
//! the error.
//!
//! # In the grader's sandbox
//!
//! A strategy component has no files, no environment, no network, and a
//! clock that stands still. Loading embedded weights needs none of them:
//! it reads the bytes it is given, on the CPU ([`DEVICE`]). What doesn't
//! work there is what reads a file (`VarBuilder::from_mmaped_safetensors`,
//! `candle::safetensors::load`, `VarMap::load`). Initialising weights at
//! random (`VarBuilder::from_varmap`) draws from the session's seeded
//! randomness, so it runs, but an untrained network is no use to a
//! strategy: train where you have the data, and embed the result.

use std::collections::HashMap;

use candle_core::{DType, Device, Error, Result, Tensor};
use candle_nn::VarBuilder;

/// Where a strategy's tensors live: the CPU, the only device a WebAssembly
/// component has.
pub const DEVICE: Device = Device::Cpu;

/// The longest weights file loaded: 64 MiB, 16 million `f32` parameters. A
/// network that evaluates within a step's budget is a small fraction of
/// that, and a component's memory is capped at 1 GiB.
pub const WEIGHTS_BYTES_MAX: usize = 64 * 1024 * 1024;

/// A `VarBuilder` over safetensors bytes, to build candle layers from:
/// `candle_nn::linear(8, 32, weights.pp("hidden"))`. The tensors are `f32`
/// on the CPU, and are read from `weights` as each layer asks for them,
/// without copying the file.
pub fn var_builder(weights: &[u8]) -> Result<VarBuilder<'_>> {
    check_len(weights)?;
    VarBuilder::from_slice_safetensors(weights, DType::F32, &DEVICE)
}

/// Every tensor in safetensors bytes, by name, on the CPU: for a model
/// written with tensor operations instead of `candle_nn` layers.
pub fn tensors(weights: &[u8]) -> Result<HashMap<String, Tensor>> {
    check_len(weights)?;
    candle_core::safetensors::load_buffer(weights, &DEVICE)
}

fn check_len(weights: &[u8]) -> Result<()> {
    if weights.len() > WEIGHTS_BYTES_MAX {
        return Err(Error::Msg(format!(
            "{} bytes of weights is over the maximum, {WEIGHTS_BYTES_MAX}",
            weights.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use candle_core::Module;

    use super::*;

    /// A safetensors file, written by hand: a `u64` header length, a JSON
    /// header naming each tensor's type, shape and byte range, then the
    /// data. Tensors are laid out in the order given.
    fn safetensors(tensors: &[(&str, &[usize], &[f32])]) -> Vec<u8> {
        let mut entries = Vec::new();
        let mut data = Vec::new();
        for (name, shape, values) in tensors {
            let start = data.len();
            for value in *values {
                data.extend(value.to_le_bytes());
            }
            entries.push(format!(
                r#""{name}":{{"dtype":"F32","shape":{shape:?},"data_offsets":[{start},{}]}}"#,
                data.len()
            ));
        }
        let header = format!("{{{}}}", entries.join(","));
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend(header.as_bytes());
        bytes.extend(data);
        bytes
    }

    /// A 2-to-1 linear layer: `y = 3 a - 2 b + 0.5`.
    fn linear_weights() -> Vec<u8> {
        safetensors(&[
            ("out.weight", &[1, 2], &[3.0, -2.0]),
            ("out.bias", &[1], &[0.5]),
        ])
    }

    #[test]
    fn a_layer_is_built_from_embedded_bytes() {
        let bytes = linear_weights();
        let weights = var_builder(&bytes).unwrap();
        let layer = candle_nn::linear(2, 1, weights.pp("out")).unwrap();
        let input = Tensor::new(&[[1.0f32, 1.0], [2.0, -1.0]], &DEVICE).unwrap();
        let output = layer.forward(&input).unwrap();
        assert_eq!(output.to_vec2::<f32>().unwrap(), [[1.5], [8.5]]);
    }

    #[test]
    fn tensors_are_loaded_by_name() {
        let loaded = tensors(&linear_weights()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["out.weight"].dims(), [1, 2]);
        assert_eq!(loaded["out.bias"].to_vec1::<f32>().unwrap(), [0.5]);
    }

    #[test]
    fn a_missing_tensor_or_wrong_shape_is_an_error() {
        let bytes = linear_weights();
        let weights = var_builder(&bytes).unwrap();
        assert!(candle_nn::linear(2, 1, weights.pp("hidden")).is_err());
        assert!(candle_nn::linear(3, 1, weights.pp("out")).is_err());
    }

    #[test]
    fn bytes_that_are_not_safetensors_are_an_error() {
        let valid = linear_weights();
        for bad in [&b""[..], &b"not safetensors"[..], &valid[..valid.len() - 1]] {
            assert!(var_builder(bad).is_err());
            assert!(tensors(bad).is_err());
        }
    }

    #[test]
    fn weights_over_the_limit_are_refused() {
        let long = vec![0u8; WEIGHTS_BYTES_MAX + 1];
        let error = var_builder(&long).err().unwrap().to_string();
        assert!(error.contains("over the maximum"), "{error}");
        assert!(tensors(&long).is_err());
    }
}
