// Package groups is process groups across the cluster, with publish,
// through paddock:groups, as OTP's pg: membership is eventual, and a node
// cut off from another stops listing its members until they hear each
// other again.
//
// Declare the `paddock:groups` capability in the manifest with the groups
// the app may join and publish to. An instance leaves every group when it
// exits; an upgrade keeps its groups. Published messages arrive as casts.
package groups

import (
	"errors"

	paddock "github.com/jrobhoward/paddock/sdks/go"
	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/groups/groups"
)

var (
	// ErrAccessDenied: the capability is not granted, or the manifest does
	// not list the group for this use.
	ErrAccessDenied = errors.New("access denied")
	// ErrLimitReached: joining would exceed limits.groups_max.
	ErrLimitReached = errors.New("group limit reached")
)

func fromHost(e groups.Error) error {
	if e == groups.ErrorLimitReached {
		return ErrLimitReached
	}
	return ErrAccessDenied
}

// Join joins group; joining again does nothing.
func Join(group string) error {
	r := groups.Join(group)
	if e := r.Err(); e != nil {
		return fromHost(*e)
	}
	return nil
}

// Leave leaves group.
func Leave(group string) error {
	r := groups.Leave(group)
	if e := r.Err(); e != nil {
		return fromHost(*e)
	}
	return nil
}

// Members lists group's members on every node this node hears from.
func Members(group string) ([]paddock.Pid, error) {
	r := groups.Members(group)
	if e := r.Err(); e != nil {
		return nil, fromHost(*e)
	}
	list := *r.OK()
	out := make([]paddock.Pid, 0, list.Len())
	for _, p := range list.Slice() {
		out = append(out, paddock.Pid{Node: p.Node, ID: p.ID, Incarnation: p.Incarnation})
	}
	abi.FreeList(list)
	return out, nil
}

// Publish casts msg to every member of group, and returns how many took it.
func Publish(group string, msg paddock.Envelope) (uint32, error) {
	r := groups.Publish(group, abi.HostEnvelope(msg.ContentType, msg.Schema, msg.Body))
	if e := r.Err(); e != nil {
		return 0, fromHost(*e)
	}
	return *r.OK(), nil
}
