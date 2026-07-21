# mloader — desk-pilot model loader CLI tools.
#   mloader-serve    — 起 vLLM / SGLang / mock（OpenAI 兼容）
#   mloader-download — huggingface_hub 模型下载（断点续传 + 校验）
#   mloader-detect   — GPU / CUDA / Python 引擎环境探测
#
# pip install -e .           # 只装 download/detect
# pip install -e ".[server]" # 连 fastapi/uvicorn（mock serve 子命令用）一起装
