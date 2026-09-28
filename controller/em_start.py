"""Home Assistant add-on entrypoint.

Supervisor writes add-on options to /data/options.json; this translates those
keys into the environment names em_controller reads, then execs the controller.
Outside the add-on the file does not exist and this is a passthrough.

Home Assistant credentials are intentionally not options: `homeassistant_api:
true` in config.yaml makes Supervisor inject `SUPERVISOR_TOKEN`, which
em_ha_client uses with `ws://supervisor/core/websocket` (SPEC §16.7).
"""

import json
import os
from pathlib import Path

OPTIONS_PATH = Path("/data/options.json")

# Explicit so a renamed/removed add-on option cannot silently reach the
# controller under a name it never reads.
OPTION_ENV_VARS = {
    "server_host": "SERVER_HOST",
    "server_ip": "SERVER_IP",
    "mdns_name": "MDNS_NAME",
    "esphome_project_version": "ESPHOME_PROJECT_VERSION",
    "require_device_tls": "REQUIRE_DEVICE_TLS",
    "device_approval": "DEVICE_APPROVAL",
}

if OPTIONS_PATH.is_file():
    options = json.loads(OPTIONS_PATH.read_text(encoding="utf-8"))
    for key, value in options.items():
        env_key = OPTION_ENV_VARS.get(key)
        if env_key is None:
            print(f"em_start: no env var mapped for add-on option {key!r} — "
                  "config.yaml and OPTION_ENV_VARS have drifted", flush=True)
            continue
        if value == "":
            continue
        env_value = "1" if value is True else "0" if value is False else str(value)
        os.environ.setdefault(env_key, env_value)

os.execvp("python3", ["python3", "-u", "em_controller.py"])
