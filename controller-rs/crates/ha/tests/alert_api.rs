//! The calendar, Local Calendar, script and event operations the alert service uses,
//! against a simulated HA speaking HA 2026.8.1's message shapes.

mod fake_ha;

use std::time::Duration;

use echomuse_core::services::{
    EffectOutcome, HaConnectionGeneration, HaError, NotDispatched, UnknownCause,
};
use echomuse_ha::{
    CalendarItem, CalendarPush, CalendarTime, ConfigEntry, EventTarget, EventWrite, HaClient,
    HaEndpoint, HaOptions,
};
use fake_ha::{FakeConnection, FakeHa, TOKEN, within};
use serde_json::{Value, json};

fn options() -> HaOptions {
    HaOptions {
        command_timeout: Duration::from_secs(2),
        reconnect_initial: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(200),
        heartbeat: Duration::from_secs(60),
        stt_reconnect_wait: Duration::from_secs(2),
        subscription_capacity: 4,
    }
}

async fn session() -> (HaClient, FakeHa, FakeConnection) {
    let mut ha = FakeHa::start().await;
    let endpoint = HaEndpoint::direct(&ha.base_url(), TOKEN).unwrap();
    let (client, _task) = HaClient::start(endpoint, options()).unwrap();
    let connection = ha.connection().await;
    let mut status = client.status();
    within(status.wait_for(|s| s.generation == Some(HaConnectionGeneration(1))))
        .await
        .unwrap();
    (client, ha, connection)
}

const CALENDAR: &str = "calendar.echomuse_office";

fn wake_up() -> EventWrite {
    EventWrite {
        summary: "Wake up".to_owned(),
        dtstart: "2026-09-23T06:30:00-05:00".to_owned(),
        dtend: "2026-09-23T06:31:00-05:00".to_owned(),
        description: Some("echomuse: {}".to_owned()),
        rrule: Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".to_owned()),
    }
}

#[tokio::test]
async fn an_event_is_created_and_deleted_with_the_stock_commands() {
    let (client, _ha, mut connection) = session().await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.calendar_create_event(CALENDAR, &wake_up()).await }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "calendar/event/create", "entity_id": CALENDAR, "id": request["id"],
               "event": {"summary": "Wake up", "dtstart": "2026-09-23T06:30:00-05:00",
                         "dtend": "2026-09-23T06:31:00-05:00", "description": "echomuse: {}",
                         "rrule": "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"}})
    );
    connection.result(&request["id"], Value::Null);
    assert_eq!(within(call).await.unwrap(), EffectOutcome::Applied(()));

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            let target = EventTarget {
                uid: "u1".to_owned(),
                recurrence_id: Some("20260924T063000".to_owned()),
            };
            client.calendar_delete_event(CALENDAR, &target).await
        }
    });
    let request = connection.request().await;
    assert_eq!(request["type"], "calendar/event/delete");
    assert_eq!(request["uid"], "u1");
    assert_eq!(request["recurrence_id"], "20260924T063000");
    connection.error(
        &request["id"],
        "failed",
        "Error while deleting event: not found",
    );
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Rejected {
            code: "failed".to_owned(),
            message: "Error while deleting event: not found".to_owned()
        }
    );

    // A whole series is deleted without recurrence_id; a lost reply is unknown.
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            let target = EventTarget {
                uid: "u1".to_owned(),
                recurrence_id: None,
            };
            client.calendar_delete_event(CALENDAR, &target).await
        }
    });
    let request = connection.request().await;
    assert!(request.get("recurrence_id").is_none());
    connection.close();
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Unknown(UnknownCause::ConnectionLost)
    );
}

#[tokio::test]
async fn a_subscription_delivers_each_full_push_until_the_connection_ends() {
    let (client, _ha, mut connection) = session().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .calendar_subscribe(
                    CALENDAR,
                    "2026-09-23T06:00:00-05:00",
                    "2026-09-30T06:30:00-05:00",
                )
                .await
        }
    });
    let request = connection.request().await;
    assert_eq!(request["type"], "calendar/event/subscribe");
    assert_eq!(request["start"], "2026-09-23T06:00:00-05:00");
    assert_eq!(request["end"], "2026-09-30T06:30:00-05:00");
    let id = request["id"].clone();
    connection.result(&id, Value::Null);
    let mut subscription = within(call).await.unwrap().unwrap();
    assert_eq!(subscription.generation, HaConnectionGeneration(1));

    connection.event(
        &id,
        json!({"events": [{"start": "2026-09-23T06:30:00-05:00", "end": "2026-09-23T06:31:00-05:00",
                           "summary": "Wake up", "uid": "u1", "all_day": false}]}),
    );
    connection.event(&id, json!({"events": null}));
    assert_eq!(
        within(subscription.next()).await,
        Some(CalendarPush::Events(vec![CalendarItem {
            summary: "Wake up".to_owned(),
            start: CalendarTime::DateTime("2026-09-23T06:30:00-05:00".to_owned()),
            end: CalendarTime::DateTime("2026-09-23T06:31:00-05:00".to_owned()),
            description: None,
            uid: Some("u1".to_owned()),
            recurrence_id: None,
            rrule: None,
        }]))
    );
    assert_eq!(
        within(subscription.next()).await,
        Some(CalendarPush::Unavailable)
    );

    let unsubscribe = tokio::spawn({
        let client = client.clone();
        async move { client.calendar_unsubscribe(subscription).await }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "unsubscribe_events", "subscription": id, "id": request["id"]})
    );
    connection.result(&request["id"], Value::Null);
    assert_eq!(within(unsubscribe).await.unwrap(), Ok(()));
}

#[tokio::test]
async fn a_refused_subscription_is_an_error_and_a_lost_one_ends() {
    let (client, _ha, mut connection) = session().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .calendar_subscribe(
                    "calendar.gone",
                    "2026-09-23T06:00:00Z",
                    "2026-09-30T06:00:00Z",
                )
                .await
        }
    });
    let request = connection.request().await;
    connection.error(
        &request["id"],
        "not_found",
        "Calendar entity not found: calendar.gone",
    );
    assert!(matches!(
        within(call).await.unwrap(),
        Err(HaError::Rejected { code, .. }) if code == "not_found"
    ));

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .calendar_subscribe(CALENDAR, "2026-09-23T06:00:00Z", "2026-09-30T06:00:00Z")
                .await
        }
    });
    let request = connection.request().await;
    connection.result(&request["id"], Value::Null);
    let mut subscription = within(call).await.unwrap().unwrap();
    connection.close();
    assert_eq!(within(subscription.next()).await, None);
}

#[tokio::test]
async fn the_backlog_is_read_through_rest_with_encoded_times() {
    let (client, mut ha, _connection) = session().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .calendar_events(
                    CALENDAR,
                    "2026-06-25T11:00:00+00:00",
                    "2026-09-23T10:30:00+00:00",
                )
                .await
        }
    });
    let rest = ha.rest_call().await;
    assert_eq!(rest.method, "GET");
    assert_eq!(
        rest.path,
        "/api/calendars/calendar.echomuse_office?start=2026-06-25T11%3A00%3A00%2B00%3A00&end=2026-09-23T10%3A30%3A00%2B00%3A00"
    );
    rest.respond(
        200,
        json!([{"start": {"dateTime": "2026-09-01T07:00:00-05:00"},
                "end": {"dateTime": "2026-09-01T07:01:00-05:00"}, "summary": "Alarm",
                "description": null, "location": null, "uid": "u9",
                "recurrence_id": "20260901T070000", "rrule": "FREQ=DAILY"}]),
    );
    let items = within(call).await.unwrap().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].recurrence_id.as_deref(), Some("20260901T070000"));
}

#[tokio::test]
async fn a_local_calendar_is_found_or_created_through_the_config_flow() {
    let (client, mut ha, mut connection) = session().await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.time_zone().await }
    });
    let request = connection.request().await;
    assert_eq!(request["type"], "get_config");
    connection.result(
        &request["id"],
        json!({"time_zone": "America/Chicago", "version": "2026.8.1"}),
    );
    assert_eq!(
        within(call).await.unwrap(),
        Ok("America/Chicago".to_owned())
    );

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.config_entries("local_calendar").await }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "config_entries/get", "domain": "local_calendar", "id": request["id"]})
    );
    connection.result(
        &request["id"],
        json!([{"entry_id": "01K1", "domain": "local_calendar", "title": "EchoMuse Office",
                "state": "loaded"}]),
    );
    assert_eq!(
        within(call).await.unwrap(),
        Ok(vec![ConfigEntry {
            entry_id: "01K1".to_owned(),
            title: "EchoMuse Office".to_owned()
        }])
    );

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.entities_of_entry("01K1", "calendar").await }
    });
    let request = connection.request().await;
    assert_eq!(request["type"], "config/entity_registry/list");
    connection.result(
        &request["id"],
        json!([{"entity_id": "calendar.echomuse_office", "config_entry_id": "01K1", "platform": "local_calendar"},
               {"entity_id": "sensor.other", "config_entry_id": "01K1", "platform": "local_calendar"},
               {"entity_id": "calendar.family", "config_entry_id": "01K2", "platform": "local_calendar"}]),
    );
    assert_eq!(
        within(call).await.unwrap(),
        Ok(vec!["calendar.echomuse_office".to_owned()])
    );

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.create_local_calendar("EchoMuse Kitchen").await }
    });
    let start = ha.rest_call().await;
    assert_eq!(
        (start.method.as_str(), start.path.as_str()),
        ("POST", "/api/config/config_entries/flow")
    );
    assert_eq!(start.body["handler"], "local_calendar");
    start.respond(
        200,
        json!({"type": "form", "flow_id": "f1", "handler": "local_calendar", "step_id": "user",
               "data_schema": [], "errors": null}),
    );
    let step = ha.rest_call().await;
    assert_eq!(step.path, "/api/config/config_entries/flow/f1");
    assert_eq!(
        step.body,
        json!({"calendar_name": "EchoMuse Kitchen", "import": "create_empty"})
    );
    step.respond(
        200,
        json!({"type": "create_entry", "flow_id": "f1", "handler": "local_calendar",
               "title": "EchoMuse Kitchen",
               "result": {"entry_id": "01K3", "domain": "local_calendar", "title": "EchoMuse Kitchen"}}),
    );
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Applied(ConfigEntry {
            entry_id: "01K3".to_owned(),
            title: "EchoMuse Kitchen".to_owned()
        })
    );

    // A calendar with that storage key already exists: HA aborts the flow.
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.create_local_calendar("EchoMuse Kitchen").await }
    });
    ha.rest_call().await.respond(
        200,
        json!({"type": "form", "flow_id": "f2", "step_id": "user"}),
    );
    ha.rest_call().await.respond(
        200,
        json!({"type": "abort", "flow_id": "f2", "reason": "already_configured"}),
    );
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Rejected {
            code: "abort".to_owned(),
            message: "already_configured".to_owned()
        }
    );
}

#[tokio::test]
async fn scripts_are_read_written_reloaded_and_exposed() {
    let (client, mut ha, mut connection) = session().await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.script_config("echomuse_set_alarm").await }
    });
    let rest = ha.rest_call().await;
    assert_eq!(rest.path, "/api/config/script/config/echomuse_set_alarm");
    rest.respond(404, json!({"message": "Resource not found"}));
    assert_eq!(within(call).await.unwrap(), Ok(None));

    let config = json!({"alias": "Set an alarm", "sequence": []});
    let call = tokio::spawn({
        let client = client.clone();
        let config = config.clone();
        async move {
            client
                .write_script_config("echomuse_set_alarm", &config)
                .await
        }
    });
    let rest = ha.rest_call().await;
    assert_eq!(rest.method, "POST");
    assert_eq!(rest.body, config);
    rest.respond(200, json!({"result": "ok"}));
    assert_eq!(within(call).await.unwrap(), EffectOutcome::Applied(()));

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .write_script_config("echomuse_set_alarm", &json!({}))
                .await
        }
    });
    ha.rest_call().await.respond(
        400,
        json!({"message": "Message malformed: required key not provided"}),
    );
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Rejected {
            code: "400".to_owned(),
            message: "Message malformed: required key not provided".to_owned()
        }
    );

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.reload_scripts().await }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "call_service", "domain": "script", "service": "reload", "id": request["id"]})
    );
    connection.result(&request["id"], json!({"context": {"id": "c"}}));
    assert_eq!(within(call).await.unwrap(), EffectOutcome::Applied(()));

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .expose_to_conversation(&["script.echomuse_set_alarm".to_owned()])
                .await
        }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "homeassistant/expose_entity", "assistants": ["conversation"],
               "entity_ids": ["script.echomuse_set_alarm"], "should_expose": true,
               "id": request["id"]})
    );
    connection.result(&request["id"], Value::Null);
    assert_eq!(within(call).await.unwrap(), EffectOutcome::Applied(()));

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .fire_event(
                    "echomuse_alert_result",
                    json!({"request_id": "r", "result": {"ok": true}}),
                )
                .await
        }
    });
    let request = connection.request().await;
    assert_eq!(
        request,
        json!({"type": "fire_event", "event_type": "echomuse_alert_result",
               "event_data": {"request_id": "r", "result": {"ok": true}}, "id": request["id"]})
    );
    connection.result(&request["id"], json!({"context": {"id": "c"}}));
    assert_eq!(within(call).await.unwrap(), EffectOutcome::Applied(()));
}

#[tokio::test]
async fn nothing_is_sent_without_a_connection() {
    let (client, _ha, _connection) = session().await;
    client.stop();
    let mut status = client.status();
    within(status.wait_for(|s| s.generation.is_none()))
        .await
        .unwrap();
    assert_eq!(
        client.fire_event("echomuse_alert_result", json!({})).await,
        EffectOutcome::NotDispatched(NotDispatched::Unavailable)
    );
}

#[tokio::test]
async fn enabled_automations_conversation_triggers_are_read() {
    let (client, mut ha, _connection) = session().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.automation_sentences().await }
    });
    let rest = ha.rest_call().await;
    assert_eq!(rest.path, "/api/states");
    rest.respond(
        200,
        json!([
            {"entity_id": "automation.arm", "state": "on", "attributes": {"id": "1700"}},
            {"entity_id": "automation.off", "state": "off", "attributes": {"id": "1701"}},
            {"entity_id": "automation.yaml", "state": "on", "attributes": {}},
            {"entity_id": "light.kitchen", "state": "on", "attributes": {"id": "x"}},
        ]),
    );
    let rest = ha.rest_call().await;
    assert_eq!(rest.path, "/api/config/automation/config/1700");
    rest.respond(
        200,
        json!({"alias": "Arm", "triggers": [
            {"trigger": "conversation", "command": ["arm the alarm", "(good night|bedtime)"]},
            {"trigger": "time", "at": "22:00"},
        ]}),
    );
    assert_eq!(
        within(call).await.unwrap().unwrap(),
        ["arm the alarm", "(good night|bedtime)"]
    );
    assert_eq!(echomuse_ha::AUTOMATIONS_EVENT, "automation_reloaded");
}
