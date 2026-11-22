use std::time::Instant;

use image::RgbImage;
use openvino::{
    CompiledModel, Core, DeviceType, ElementType, InferRequest, PropertyKey, RwPropertyKey,
};

use super::{ANCHORS, EonResult, IconPage, Letterbox, SIDE, check_time, failed};

const BUILD: &str = "2026.4.1-22982-e213a147257-releases/2026/4";

pub(super) struct CpuDetector {
    request: Option<InferRequest>,
    model: CompiledModel,
}

impl CpuDetector {
    pub(super) fn load(graph: &[u8]) -> EonResult<Self> {
        let mut core = Core::new().map_err(failed)?;
        if openvino::version().build_number != BUILD {
            return Err(failed("OpenVINO runtime build mismatch"));
        }
        let caps = core
            .get_property(&DeviceType::CPU, &PropertyKey::DeviceCapabilities)
            .map_err(failed)?;
        if !caps.split_ascii_whitespace().any(|cap| cap == "BF16") {
            return Err(failed("OpenVINO CPU plugin does not advertise BF16"));
        }
        use RwPropertyKey::*;
        core.set_properties(
            &DeviceType::CPU,
            [
                (HintPerformanceMode, "LATENCY"),
                (NumStreams, "1"),
                (HintEnableHyperThreading, "NO"),
                (InferenceNumThreads, "32"),
                (HintInferencePrecision, "bf16"),
            ],
        )
        .map_err(failed)?;
        let graph = core.read_model_from_buffer(graph, None).map_err(failed)?;
        let model = core
            .compile_model(&graph, DeviceType::CPU)
            .map_err(failed)?;
        Ok(Self {
            request: None,
            model,
        })
    }

    pub(super) fn detect(&mut self, image: &RgbImage, end: Instant) -> EonResult<IconPage> {
        let mut request = match self.request.take() {
            Some(request) => request,
            None => self.model.create_infer_request().map_err(failed)?,
        };
        let fit = Letterbox::new(image.width(), image.height());
        let mut input = request.get_input_tensor().map_err(failed)?;
        if input.get_element_type().map_err(failed)? != ElementType::F32
            || input.get_shape().map_err(failed)?.get_dimensions()
                != [1, 3, SIDE.into(), SIDE.into()]
        {
            return Err(failed("unexpected OpenVINO input tensor"));
        }
        fit.fill_tensor(image, input.get_data_mut::<f32>().map_err(failed)?);
        check_time(end)?;
        request.infer_async().map_err(failed)?;
        // The native API waits in integral milliseconds. Reject overdue results
        // even in the final rounded millisecond; never reuse a timed-out request.
        let us = end.saturating_duration_since(Instant::now()).as_micros();
        if let Err(e) = request.wait(us.div_ceil(1000) as i64) {
            let _ = request.cancel();
            return Err(failed(e));
        }
        check_time(end)?;
        let output = request.get_output_tensor().map_err(failed)?;
        if output.get_element_type().map_err(failed)? != ElementType::F32
            || output.get_shape().map_err(failed)?.get_dimensions() != [1, 5, ANCHORS as i64]
        {
            return Err(failed("unexpected OpenVINO YOLO head"));
        }
        let icons = fit.detections(output.get_data::<f32>().map_err(failed)?)?;
        self.request = Some(request);
        Ok(IconPage {
            width: image.width(),
            height: image.height(),
            icons,
        })
    }
}
