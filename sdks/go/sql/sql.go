// Package sql is SQL datasources the node binds, through paddock:sql.
// Declare the `paddock:sql` capability in the manifest with the
// datasources' names. Parameters are $1, $2 ... (Postgres).
package sql

import (
	"errors"
	"fmt"

	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/sql/sql"
	"go.bytecodealliance.org/cm"
)

var (
	// ErrAccessDenied: the capability is not granted, or the manifest does
	// not list the datasource.
	ErrAccessDenied = errors.New("access denied")
	// ErrNotFound: the node binds no such datasource.
	ErrNotFound = errors.New("no such datasource")
	// ErrLimitReached: no connection free in time.
	ErrLimitReached = errors.New("no connection free")
	// ErrTimeout: the statement ran past its timeout.
	ErrTimeout = errors.New("statement timeout")
	// ErrTooLarge: the result is over max_result_bytes.
	ErrTooLarge = errors.New("result too large")
)

// UnavailableError: the database could not be reached.
type UnavailableError struct{ Msg string }

func (e *UnavailableError) Error() string { return "unavailable: " + e.Msg }

// UnsupportedError: a parameter or column type the host does not map.
type UnsupportedError struct{ Msg string }

func (e *UnsupportedError) Error() string { return "unsupported: " + e.Msg }

// QueryError is the database's error, with its SQLSTATE code.
type QueryError struct{ Code, Message string }

func (e *QueryError) Error() string { return fmt.Sprintf("%s (SQLSTATE %s)", e.Message, e.Code) }

func takeError(e sql.Error) error {
	switch {
	case e.AccessDenied():
		return ErrAccessDenied
	case e.NotFound():
		return ErrNotFound
	case e.LimitReached():
		return ErrLimitReached
	case e.Timeout():
		return ErrTimeout
	case e.TooLarge():
		return ErrTooLarge
	case e.Unavailable() != nil:
		return &UnavailableError{Msg: abi.String(*e.Unavailable())}
	case e.Unsupported() != nil:
		return &UnsupportedError{Msg: abi.String(*e.Unsupported())}
	default:
		q := e.Query()
		return &QueryError{Code: abi.String(q.Code), Message: abi.String(q.Message)}
	}
}

// Value is a parameter or a column's value: nil, bool, int64, float64,
// string or []byte. Parameters of other integer types are widened to int64.
type Value = any

func toHost(params []Value) (cm.List[sql.Value], error) {
	out := make([]sql.Value, 0, len(params))
	for i, p := range params {
		switch v := p.(type) {
		case nil:
			out = append(out, sql.ValueNull())
		case bool:
			out = append(out, sql.ValueBoolean(v))
		case int:
			out = append(out, sql.ValueInt(int64(v)))
		case int32:
			out = append(out, sql.ValueInt(int64(v)))
		case int64:
			out = append(out, sql.ValueInt(v))
		case float64:
			out = append(out, sql.ValueFloat(v))
		case string:
			out = append(out, sql.ValueText(v))
		case []byte:
			out = append(out, sql.ValueBytes(cm.ToList(v)))
		default:
			return cm.List[sql.Value]{}, &UnsupportedError{Msg: fmt.Sprintf("parameter %d is a %T", i+1, p)}
		}
	}
	return cm.ToList(out), nil
}

func takeValue(v sql.Value) Value {
	switch {
	case v.Null():
		return nil
	case v.Boolean() != nil:
		return *v.Boolean()
	case v.Int() != nil:
		return *v.Int()
	case v.Float() != nil:
		return *v.Float()
	case v.Text() != nil:
		return abi.String(*v.Text())
	default:
		return abi.Bytes(*v.Bytes())
	}
}

// Rows is a query's result: the columns' names, and each row's values in
// that order.
type Rows struct {
	Columns []string
	Rows    [][]Value
}

func takeRows(r sql.Rows) Rows {
	out := Rows{Columns: abi.Strings(r.Columns)}
	for _, row := range r.Rows.Slice() {
		values := make([]Value, 0, row.Len())
		for _, v := range row.Slice() {
			values = append(values, takeValue(v))
		}
		abi.FreeList(row)
		out.Rows = append(out.Rows, values)
	}
	abi.FreeList(r.Rows)
	return out
}

func rows(r cm.Result[sql.ErrorShape, sql.Rows, sql.Error]) (Rows, error) {
	if e := r.Err(); e != nil {
		return Rows{}, takeError(*e)
	}
	return takeRows(*r.OK()), nil
}

func count(r cm.Result[sql.ErrorShape, uint64, sql.Error]) (uint64, error) {
	if e := r.Err(); e != nil {
		return 0, takeError(*e)
	}
	return *r.OK(), nil
}

// Query runs a statement that returns rows.
func Query(datasource, statement string, params ...Value) (Rows, error) {
	p, err := toHost(params)
	if err != nil {
		return Rows{}, err
	}
	return rows(sql.Query(datasource, statement, p))
}

// Execute runs a statement and returns the rows it changed.
func Execute(datasource, statement string, params ...Value) (uint64, error) {
	p, err := toHost(params)
	if err != nil {
		return 0, err
	}
	return count(sql.Execute(datasource, statement, p))
}

// Tx is a transaction on one connection. End it with Commit or Rollback; a
// transaction dropped unended, or open when the instance exits, is rolled
// back.
type Tx struct{ tx sql.Transaction }

// Begin starts a transaction.
func Begin(datasource string) (*Tx, error) {
	r := sql.Begin(datasource)
	if e := r.Err(); e != nil {
		return nil, takeError(*e)
	}
	return &Tx{tx: *r.OK()}, nil
}

// Query runs a statement that returns rows, in the transaction.
func (t *Tx) Query(statement string, params ...Value) (Rows, error) {
	p, err := toHost(params)
	if err != nil {
		return Rows{}, err
	}
	return rows(t.tx.Query(statement, p))
}

// Execute runs a statement in the transaction.
func (t *Tx) Execute(statement string, params ...Value) (uint64, error) {
	p, err := toHost(params)
	if err != nil {
		return 0, err
	}
	return count(t.tx.Execute(statement, p))
}

// Commit commits the transaction.
func (t *Tx) Commit() error {
	r := sql.TransactionCommit(t.tx)
	if e := r.Err(); e != nil {
		return takeError(*e)
	}
	return nil
}

// Rollback rolls the transaction back.
func (t *Tx) Rollback() error {
	r := sql.TransactionRollback(t.tx)
	if e := r.Err(); e != nil {
		return takeError(*e)
	}
	return nil
}
