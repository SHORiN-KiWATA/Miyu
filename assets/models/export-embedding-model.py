#!/usr/bin/env python3
"""Export a Hugging Face sentence-embedding model to the layout Miyu loads.

    python scripts/export-embedding-model.py BAAI/bge-small-zh-v1.5 assets/models/bge-small-zh-v1.5-int8 \
        --dims 512 --pooling cls --min-score 0.35

Requires: pip install "optimum[onnxruntime]" onnx onnxruntime transformers

Steps: export to ONNX with optimum, dynamically quantize weights to int8
(the same recipe as the Xenova conversions), copy the tokenizer, and write
manifest.json with sha256 of the files. Re-running on the same upstream
revision reproduces identical files.
"""
import argparse, hashlib, json, os, shutil, subprocess, sys, tempfile


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("model")
    ap.add_argument("out")
    ap.add_argument("--dims", type=int, required=True)
    ap.add_argument("--pooling", choices=["cls", "mean"], default="cls")
    ap.add_argument("--max-length", type=int, default=512)
    ap.add_argument("--query-prefix", default="")
    ap.add_argument("--min-score", type=float, default=0.35)
    ap.add_argument("--no-quantize", action="store_true")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory() as tmp:
        subprocess.check_call([
            sys.executable, "-m", "optimum.exporters.onnx", "--model", args.model,
            "--task", "feature-extraction", tmp,
        ])
        os.makedirs(args.out, exist_ok=True)
        src = os.path.join(tmp, "model.onnx")
        dst = os.path.join(args.out, "model.onnx")
        if args.no_quantize:
            shutil.copy(src, dst)
        else:
            from onnxruntime.quantization import quantize_dynamic, QuantType
            quantize_dynamic(src, dst, weight_type=QuantType.QInt8)
        for name in ("tokenizer.json", "config.json", "special_tokens_map.json", "tokenizer_config.json"):
            path = os.path.join(tmp, name)
            if os.path.exists(path):
                shutil.copy(path, os.path.join(args.out, name))

    model_id = os.path.basename(os.path.normpath(args.out))
    manifest = {
        "id": model_id,
        "display_name": args.model,
        "source": f"https://huggingface.co/{args.model}",
        "license": "see LICENSE",
        "model_file": "model.onnx",
        "tokenizer_file": "tokenizer.json",
        "sha256": {
            "model.onnx": sha256(dst),
            "tokenizer.json": sha256(os.path.join(args.out, "tokenizer.json")),
        },
        "dims": args.dims,
        "pooling": args.pooling,
        "normalize": True,
        "max_length": args.max_length,
        "query_prefix": args.query_prefix,
        "min_score": args.min_score,
    }
    with open(os.path.join(args.out, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print("wrote", args.out)


if __name__ == "__main__":
    main()
