/* Keep the release-specific config and result layouts in C, not in Rust. */
#include "sherpa-onnx/c-api/c-api.h"
#include <string.h>

const SherpaOnnxOnlineRecognizer *eon_sherpa_create(
    const char *encoder, const char *decoder, const char *joiner, const char *tokens) {
  if (strcmp(SherpaOnnxGetVersionStr(), "1.12.20") != 0)
    return NULL;
  SherpaOnnxOnlineRecognizerConfig config = {0};
  config.feat_config.sample_rate = 16000;
  config.feat_config.feature_dim = 80;
  config.model_config.transducer.encoder = encoder;
  config.model_config.transducer.decoder = decoder;
  config.model_config.transducer.joiner = joiner;
  config.model_config.tokens = tokens;
  config.model_config.num_threads = 1;
  config.model_config.provider = "cpu";
  config.decoding_method = "greedy_search";
  return SherpaOnnxCreateOnlineRecognizer(&config);
}

const char *eon_sherpa_text(const SherpaOnnxOnlineRecognizerResult *result) {
  return result->text;
}
