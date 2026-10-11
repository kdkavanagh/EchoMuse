"""
Controller tests need pytest, numpy and scipy, plus websockets and aiohttp
for the device-link and HA-client tests. Tests that load
real models skip without onnxruntime/sherpa-onnx; nothing needs a live
Home Assistant or a device. Run from anywhere:

    cd controller && python -m pytest
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
