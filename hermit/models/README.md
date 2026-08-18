# Wake-word models — "Hey Sudo"

Three ONNX graphs, run in sequence by `src/speech/wake_onnx.rs`:

| file | origin | role |
|---|---|---|
| `melspectrogram.onnx` | stock livekit-wakeword resource | audio -> (time, 32) dB mel |
| `embedding_model.onnx` | stock livekit-wakeword resource | (1,76,32,1) -> (96,) embedding |
| `hey_sudo.onnx` | **trained by this project**, from `heysudo/sudo` at `sudoedge/models/hey_sudo.onnx` | (1,16,96) -> score |

`hey_sudo.onnx` is 97 KB / 18,721 parameters. The other two are the unmodified
upstream feature extractors that the classifier was trained against — swapping them
for different versions will silently wreck accuracy.

Deployed to `/opt/hermit/models/` (override with `HERMIT_MODEL_DIR`).

Retraining is documented in the `heysudo/sudo` repo at `docs/wake-training.md`. If a
new `hey_sudo.onnx` is exported, the 2.0 s window and 80 ms frame here must stay in
step with `WINDOW_FRAMES` there.
