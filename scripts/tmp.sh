echo "=== huggingface-cli 下载 1.7B (~3.5GB, 后台) ==="
mkdir -p native/models/qwen3-asr-1.7b-hf
uv run huggingface-cli download Qwen/Qwen3-ASR-1.7B --local-dir native/models/qwen3-asr-1.7b-hf 2>&1 | tail -5
echo "EXIT=${PIPESTATUS[0]}"
du -sh native/models/qwen3-asr-1.7b-hf/
echo "=== 本地路径加载 + 测延迟 ==="
uv run python -c "
import time, torch
from qwen_asr import Qwen3ASRModel
m = Qwen3ASRModel.from_pretrained('native/models/qwen3-asr-1.7b-hf', dtype=torch.float16, device_map='cuda', max_inference_batch_size=1, max_new_tokens=256)
wav = 'native/models/testwavs/zh-standard-0.wav'
for _ in range(2): m.transcribe(wav)
ts=[]
for _ in range(5):
    t0 = time.time()
    r = m.transcribe(wav)
    ms = (time.time()-t0)*1000
    ts.append(ms)
    print(f'  {ms:.0f}ms: {r[0].text[:80]}')
ts.sort(); n=len(ts)
print(f'min={ts[0]:.0f}ms p50={ts[n//2]:.0f}ms mean={sum(ts)//n:.0f}ms')" 2>&1
echo "GPU: $(nvidia-smi --query-gpu=memory.used --format=csv,noheader)"