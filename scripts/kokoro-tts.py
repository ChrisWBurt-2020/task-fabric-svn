#!/usr/bin/env python3
"""Small bounded stdin-to-WAV adapter for the local kokoro-onnx runtime."""

from __future__ import annotations

import argparse
import sys
import wave

import numpy as np
from kokoro_onnx import Kokoro


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", action="version", version="task-fabric-svn-kokoro-tts 1")
    parser.add_argument("--model", required=True)
    parser.add_argument("--voices", required=True)
    parser.add_argument("--voice", default="af_heart")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    text = sys.stdin.read(512).strip()
    if not text:
        return 2
    engine = Kokoro(args.model, args.voices)
    if args.voice not in engine.voices:
        return 3
    audio, sample_rate = engine.create(text, args.voice, speed=1.0, lang="en-us")
    pcm = (np.clip(audio, -1.0, 1.0) * 32767).astype(np.int16)
    with wave.open(args.output, "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(sample_rate)
        output.writeframes(pcm.tobytes())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
