//! Building `ort` sessions and checking a model's inputs and outputs against the contract this
//! crate was written for.

use ort::session::Session;
use ort::value::{TensorElementType, ValueType};

use crate::digest::PinnedFile;
use crate::error::SpeechError;
use crate::native::OrtRuntime;

/// One dimension of an expected tensor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dim {
    /// The model must declare exactly this size.
    Exactly(i64),
    /// A batch dimension: declared as 1 or left dynamic.
    Batch,
    /// Anything, including dynamic.
    Any,
}

/// An expected named tensor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TensorSpec {
    pub name: &'static str,
    pub ty: TensorElementType,
    pub dims: &'static [Dim],
}

/// Reads and verifies the model file, then builds a single-threaded session from those exact
/// bytes.
pub(crate) fn load_session(
    runtime: &OrtRuntime,
    file: &PinnedFile,
) -> Result<Session, SpeechError> {
    let bytes = file.read_verified()?;
    let session = Session::builder(runtime.environment())?
        .with_intra_threads(1)
        .map_err(ort::Error::from)?
        .with_inter_threads(1)
        .map_err(ort::Error::from)?
        .with_parallel_execution(false)
        .map_err(ort::Error::from)?
        .commit_from_memory(&bytes)?;
    Ok(session)
}

fn check_one(
    model: &'static str,
    role: &str,
    index: usize,
    actual_name: &str,
    actual: &ValueType,
    spec: &TensorSpec,
) -> Result<(), SpeechError> {
    let fail = |detail: String| Err(SpeechError::ModelContract { model, detail });
    if actual_name != spec.name {
        return fail(format!(
            "{role} {index} is named {actual_name:?}, expected {:?}",
            spec.name
        ));
    }
    let ValueType::Tensor { ty, shape, .. } = actual else {
        return fail(format!(
            "{role} {:?} is {actual:?}, expected a tensor",
            spec.name
        ));
    };
    if *ty != spec.ty {
        return fail(format!(
            "{role} {:?} has element type {ty:?}, expected {:?}",
            spec.name, spec.ty
        ));
    }
    let dims: &[i64] = shape;
    if dims.len() != spec.dims.len() {
        return fail(format!(
            "{role} {:?} has shape {dims:?}, expected rank {}",
            spec.name,
            spec.dims.len()
        ));
    }
    for (i, (&d, want)) in dims.iter().zip(spec.dims).enumerate() {
        let ok = match want {
            Dim::Exactly(n) => d == *n,
            Dim::Batch => d == 1 || d < 0,
            Dim::Any => true,
        };
        if !ok {
            return fail(format!(
                "{role} {:?} dimension {i} is {d}, expected {want:?} (shape {dims:?})",
                spec.name
            ));
        }
    }
    Ok(())
}

/// Fails unless the session's inputs and outputs are exactly `inputs` and `outputs`, in order.
pub(crate) fn check_io(
    model: &'static str,
    session: &Session,
    inputs: &[TensorSpec],
    outputs: &[TensorSpec],
) -> Result<(), SpeechError> {
    for (role, actual, expected) in [
        ("input", session.inputs(), inputs),
        ("output", session.outputs(), outputs),
    ] {
        if actual.len() != expected.len() {
            let names: Vec<&str> = actual.iter().map(|o| o.name()).collect();
            return Err(SpeechError::ModelContract {
                model,
                detail: format!(
                    "{} {role}s {names:?}, expected {}",
                    actual.len(),
                    expected.len()
                ),
            });
        }
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            check_one(model, role, i, a.name(), a.dtype(), e)?;
        }
    }
    Ok(())
}

/// Fails if any value is NaN or infinite.
pub(crate) fn check_finite(
    model: &'static str,
    what: &str,
    values: &[f32],
) -> Result<(), SpeechError> {
    match values.iter().position(|v| !v.is_finite()) {
        None => Ok(()),
        Some(i) => Err(SpeechError::InvalidOutput {
            model,
            detail: format!("{what}[{i}] is {}", values[i]),
        }),
    }
}

/// Extracts an `f32` output tensor, checking its element count and that it is finite.
pub(crate) fn extract_f32<'a>(
    model: &'static str,
    name: &str,
    value: &'a ort::value::DynValue,
    expected_len: usize,
) -> Result<&'a [f32], SpeechError> {
    let (_, data) = value.try_extract_tensor::<f32>()?;
    if data.len() != expected_len {
        return Err(SpeechError::InvalidOutput {
            model,
            detail: format!("{name} has {} values, expected {expected_len}", data.len()),
        });
    }
    check_finite(model, name, data)?;
    Ok(data)
}
