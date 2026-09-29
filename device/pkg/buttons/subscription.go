package buttons

import (
	"context"
)

type ButtonClickCallback func(event ButtonClickEvent)

type ButtonClickEvent struct {
	Button    Button    `json:"button"`
	ClickType ClickType `json:"clickType"`
	Down      bool      `json:"down"`
	// HeldMs is how long the button was held, set on RELEASE only (0 on the
	// press). It exists so the controller can tell a tap from a hold without
	// timing the two messages itself — the link has been measured with RTT
	// excursions past 1600ms, which would make controller-side timing of a
	// ~750ms gesture report noise as intent.
	HeldMs int64 `json:"heldMs,omitempty"`
}

type EventSubscription struct {
	cancel context.CancelFunc
}

func NewEventSubscription(cancelFunc context.CancelFunc) *EventSubscription {
	return &EventSubscription{
		cancel: cancelFunc,
	}
}

func (e *EventSubscription) Cancel() {
	e.cancel()
}
