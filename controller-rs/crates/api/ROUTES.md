# Dashboard requests and the routes that serve them

Every request `controller/static/index.html`, `dashboard.html` and `dashboard.jsx` make
(dashboard.html makes none of its own; it loads `dashboard.jsx`), compared with the routes
`crates/api` registers. Fields are those the dashboard sends or reads. Status: **served**
(implemented with the dashboard's shapes), **refused** (registered, answers an explicit
error the dashboard shows).

| Method and path | Sent | Read | Status |
|---|---|---|---|
| `POST api/setup` (index.html) | `token, username, password` | `token, role` | served |
| `POST api/auth/login` (index.html) | `username, password` | `token, role` | served |
| `GET api/auth/me` (index.html) | Bearer | status only | served |
| `GET api/system/setup-state` (index.html) | | `needs_setup` | served |
| `POST /api/auth/logout` | `{}` | | served |
| `POST /api/auth/change-password` | `current_password, new_password` | | served |
| `WS /api/events?token=` | | `snapshot`, `device_update`, `device_log`, `alerts`, `ha_status`, `turn_complete`, connection, approval, deletion, update events | served (`controller_update` never sent: no release check) |
| `GET /api/devices` | | device objects | served |
| `PATCH /api/devices/{id}` | `label` | | served |
| `DELETE /api/devices/{id}` | | | served |
| `POST /api/devices/{id}/approve` | `label` | | served |
| `POST /api/devices/{id}/config` | settings body | | served |
| `GET /api/devices/{id}/logs?limit=50` | | log rows | served |
| `GET /api/devices/{id}/turns?limit=`, `/api/turns?limit=` | | turn rows | served |
| `GET /api/devices/{id}/near_misses?limit=`, `/api/near_misses?limit=` | | near-miss rows (`windows`) | served |
| `PATCH /api/devices/{id}/near_misses/{nm}` | `verdict` | | served |
| `GET /api/devices/{id}/turns/{t}/{trace,afe}` | | trace, AFE series | served |
| `GET /api/devices/{id}/turns/{t}/{wake,audio,recording}` | | WAV | served |
| `GET /api/devices/{id}/near_misses/{nm}/{recording,afe}` | | WAV, AFE series | served |
| `GET /api/devices/{id}/wakeclips.zip` | | ZIP | served |
| `GET /api/wakeclips.zip?min&max[&device][&crop[&window_min]]` | | ZIP; 404 `error` | served, crops included (`recordings` module docs) |
| `GET /api/near_misses.zip?min&max[&device][&verdict][&crop[&window_min]]` | | ZIP; 404 `error` | served, crops included |
| `GET /api/clip_windows` | | `pending` (null: unknown) | served (count from the clip scorer) |
| `GET /api/devices/{id}/wake_shadow?days=7` | | shadow table | served |
| `GET /api/devices/{id}/response_latency` | | percentiles | served |
| `POST /api/devices/{id}/wifi/scan`, `POST …/wifi` | `{}`; `ssid, psk` | networks | served (capability gap refused with reason) |
| `GET/DELETE /api/devices/{id}/samples[/{name}]`, `GET …/samples/{name}[/afe]`, `GET …/samples.zip`, `POST …/collect` | `enabled` | listing, WAV, sidecar, ZIP | served |
| `GET/POST/DELETE /api/devices/{id}/ambient[/{name}[/afe]]` | `enabled` | listing, WAV, sidecar | served |
| `GET /api/devices/{id}/alerts`, `POST …/alarms`, `POST …/alarms/cancel` | alarm fields | panel, `{ok, error, pending…}` | served |
| `GET /api/ha/status` | | `features, calendar_url` | served |
| `POST /api/devices/{id}/kiosk_login` | `password` | | served |
| `POST/DELETE /api/devices/{id}/update/queue`, `POST …/update`, `…/rollback`, `…/secure_link`, `…/debloat` | `{}` | | served |
| `GET /api/provision/{start_script,crown_lists,firmware}`, `POST …/tls_credentials`, `POST …/diagnostics` | wizard bodies | files, headers | served |
| `POST /api/wake_models/upload`, `DELETE /api/wake_models/{sha}`, `GET /api/wake_models` | multipart | `active, models` | served |
| `GET /api/speech/wyoming?host&port` | | `programs[].name, .version, .models[].name`; error `error` | served (admin; `crates/wyoming` client) |
| `GET /api/text_fix/unknown_words[?device_id]` | | `words[{word,count}]`; code `not_ready` | served |
| `POST /api/text_fix/trial` | `text, wake, device_id?` | `record, route, heard_words, proposed_words, satellite, language, wake_phrase, mode` | served |
| `GET /api/sounds`, `POST /api/sounds/upload`, `DELETE /api/sounds/{id}` | multipart | catalog | served |
| `POST /api/devices/{id}/sounds/{preview,stop}` | `sound_id` | | served |
| `POST /api/firmware/deploy`, `GET /api/firmware` | | report, `version` | served |
| `GET /api/support/bundle` | | JSON blob | served (admin, sanitised) |
| `POST /api/global/config`, `GET /api/global/config` | settings | settings | served |
| `GET /api/system/status` | | `controller_version` | served |
| `GET /api/releases/controller` | | `available, version, current, notes, release_url` | served: running version, no remote check (`checked: false`) |
| `WS /api/devices/{id}/shell?token=` | frames | frames | served |

Routes of spec §3.4 the dashboard does not call, and their status:

| Route | Status |
|---|---|
| `GET /api/devices/pending`, `GET /api/devices/{id}`, `GET …/config`, `GET/PATCH /api/system/config` | served |
| `GET /api/devices/{id}/activity?days=N` | served: hourly `device_metrics` and `wake_counters` summaries (`device_activity` module docs) |
| `GET /api/devices/{id}/speech_assets` | served: runtime, graph and sidecar a `session.ready` names, and whether the hello listed each as installed |
| `GET/POST /api/devices/{id}/capture`, `POST …/capture/window`, `POST …/capture/window/stop`, `GET …/capture/recording` | served (`capture` module, playback-capture.md) |
| `…/session_recorder` routes | served |
| `DELETE /api/devices/{id}/wakeclips` | refused: 501 `not_applicable` with the reason (no records command deletes wake clips consistently with publication and retention; retention keeps the newest 1,000, deleting the device removes them) |

No dashboard request, and no §3.4 route, is left unregistered. The new routes are tested
over real HTTP in `crates/api/tests/diagnostics.rs`; capture windows in the controller's
`tests/capture.rs`.
