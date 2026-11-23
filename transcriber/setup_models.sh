#!/bin/sh
# One-time: convert the Whisper models transcribe.py uses to OpenVINO int8, in $CALLSCRIBE_MODELS
# (default ~/.cache/callscribe/models). About 2.5 GB.
set -e
dir="${CALLSCRIBE_MODELS:-$HOME/.cache/callscribe/models}"
mkdir -p "$dir" && cd "$dir"
export_ov() {  # <hf model> <output dir>
  [ -f "$2/openvino_encoder_model.xml" ] && { echo "$2 exists"; return; }
  uv run --python 3.12 --with 'optimum-intel[openvino]' --with 'transformers<4.58' --with nncf \
    optimum-cli export openvino --model "$1" --task automatic-speech-recognition-with-past --weight-format int8 "$2"
}
export_ov ivrit-ai/whisper-large-v3 ivrit-large-v3-ov-int8              # Hebrew (and mixed Hebrew/English)
export_ov openai/whisper-large-v3-turbo whisper-large-v3-turbo-ov-int8  # English + language detection
