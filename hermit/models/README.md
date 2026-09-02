# Wake-word models — "Hey Sudo"

Three ONNX graphs, run in sequence by `src/speech/wake_onnx.rs`:

| file | origin | role |
|---|---|---|
| `melspectrogram.onnx` | openWakeWord feature extractor (Apache-2.0), as shipped by livekit-wakeword | audio -> (time, 32) dB mel |
| `embedding_model.onnx` | openWakeWord feature extractor (Apache-2.0), as shipped by livekit-wakeword | (1,76,32,1) -> (96,) embedding |
| `hey_sudo.onnx` | **trained by this project** with livekit-wakeword | (1,16,96) -> score |

`hey_sudo.onnx` is 97 KB / 18,721 parameters. The other two are the unmodified
upstream feature extractors that the classifier was trained against — swapping them
for different versions will silently wreck accuracy.

Deployed to `/opt/hermit/models/` (override with `HERMIT_MODEL_DIR`).

Retraining follows the standard livekit-wakeword / openWakeWord recipe (the project's
training notes are kept privately). If a new `hey_sudo.onnx` is exported, the 2.0 s window and 80 ms frame here must stay in
step with `WINDOW_FRAMES` there.
