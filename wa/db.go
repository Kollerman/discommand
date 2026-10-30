package main

import (
	"context"
	"database/sql"
	"strconv"
	"strings"
)

// Tables owned by discommand, stored next to whatsmeow's own tables.
const schema = `
CREATE TABLE IF NOT EXISTS dc_chats (
	jid      TEXT PRIMARY KEY,
	name     TEXT NOT NULL DEFAULT '',
	is_group INTEGER NOT NULL DEFAULT 0,
	last_ts  INTEGER NOT NULL DEFAULT 0,
	read_ts  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS dc_messages (
	chat        TEXT NOT NULL,
	id          TEXT NOT NULL,
	sender      TEXT NOT NULL,
	sender_name TEXT NOT NULL,
	from_me     INTEGER NOT NULL,
	ts          INTEGER NOT NULL,
	kind        TEXT NOT NULL,
	text        TEXT NOT NULL,
	file        TEXT NOT NULL DEFAULT '',
	reply_id    TEXT NOT NULL DEFAULT '',
	reply_name  TEXT NOT NULL DEFAULT '',
	reply_text  TEXT NOT NULL DEFAULT '',
	reply_sender TEXT NOT NULL DEFAULT '',
	edited      INTEGER NOT NULL DEFAULT 0,
	deleted     INTEGER NOT NULL DEFAULT 0,
	raw         BLOB,
	PRIMARY KEY (chat, id)
);
CREATE INDEX IF NOT EXISTS dc_messages_chat_ts ON dc_messages (chat, ts);
CREATE TABLE IF NOT EXISTS dc_reactions (
	chat   TEXT NOT NULL,
	msg_id TEXT NOT NULL,
	sender TEXT NOT NULL,
	emoji  TEXT NOT NULL,
	PRIMARY KEY (chat, msg_id, sender)
);
CREATE TABLE IF NOT EXISTS dc_kv (
	k TEXT PRIMARY KEY,
	v TEXT NOT NULL
);
`

// Msg is the message format sent to the terminal UI.
type Msg struct {
	ID         string `json:"id"`
	Chat       string `json:"chat"`
	Sender     string `json:"sender"`
	SenderName string `json:"sender_name"`
	FromMe     bool   `json:"from_me"`
	TS         int64  `json:"ts"`
	Kind       string `json:"kind"`
	Text       string `json:"text"`
	File       string `json:"file,omitempty"`
	ReplyID    string `json:"reply_id,omitempty"`
	ReplyName  string `json:"reply_name,omitempty"`
	ReplyText  string `json:"reply_text,omitempty"`
	// JID of the quoted sender, used to resolve ReplyName.
	ReplySender string `json:"-"`
	Edited      bool   `json:"edited,omitempty"`
	Deleted     bool   `json:"deleted,omitempty"`
	Media       bool   `json:"media,omitempty"`
	Reactions   string `json:"reactions,omitempty"`
	ImgW        uint32 `json:"img_w,omitempty"`
	ImgH        uint32 `json:"img_h,omitempty"`

	raw []byte
}

// Chat is the chat-list entry sent to the terminal UI.
type Chat struct {
	JID     string `json:"jid"`
	Name    string `json:"name"`
	IsGroup bool   `json:"is_group"`
	LastTS  int64  `json:"last_ts"`
	Unread  int    `json:"unread"`
	Pinned  int64  `json:"pinned"`
}

type DB struct {
	*sql.DB
}

func (db DB) init(ctx context.Context) error {
	if _, err := db.ExecContext(ctx, schema); err != nil {
		return err
	}
	// Added after the first release; fails harmlessly when the column exists.
	_, _ = db.ExecContext(ctx, `ALTER TABLE dc_messages ADD COLUMN reply_sender TEXT NOT NULL DEFAULT ''`)
	// When the chat was pinned (0: not pinned).
	_, _ = db.ExecContext(ctx, `ALTER TABLE dc_chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0`)
	return nil
}

func (db DB) kv(ctx context.Context, k string) string {
	var v string
	_ = db.QueryRowContext(ctx, `SELECT v FROM dc_kv WHERE k = ?`, k).Scan(&v)
	return v
}

func (db DB) setKV(ctx context.Context, k, v string) error {
	_, err := db.ExecContext(ctx, `INSERT INTO dc_kv (k, v) VALUES (?, ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v`, k, v)
	return err
}

// insertMsg stores a message; an existing row with the same id is kept.
func (db DB) insertMsg(ctx context.Context, m *Msg) (bool, error) {
	res, err := db.ExecContext(ctx, `
		INSERT INTO dc_messages (chat, id, sender, sender_name, from_me, ts, kind, text, file, reply_id, reply_name, reply_text, reply_sender, edited, deleted, raw)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT (chat, id) DO NOTHING`,
		m.Chat, m.ID, m.Sender, m.SenderName, m.FromMe, m.TS, m.Kind, m.Text, m.File,
		m.ReplyID, m.ReplyName, m.ReplyText, m.ReplySender, m.Edited, m.Deleted, m.raw)
	if err != nil {
		return false, err
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

const msgColumns = `chat, id, sender, sender_name, from_me, ts, kind, text, file, reply_id, reply_name, reply_text, reply_sender, edited, deleted, raw`

func scanMsg(row interface{ Scan(...any) error }) (*Msg, error) {
	var m Msg
	err := row.Scan(&m.Chat, &m.ID, &m.Sender, &m.SenderName, &m.FromMe, &m.TS, &m.Kind, &m.Text, &m.File,
		&m.ReplyID, &m.ReplyName, &m.ReplyText, &m.ReplySender, &m.Edited, &m.Deleted, &m.raw)
	if err != nil {
		return nil, err
	}
	m.Media = len(m.raw) > 0 && !m.Deleted
	return &m, nil
}

func (db DB) getMsg(ctx context.Context, chat, id string) (*Msg, error) {
	m, err := scanMsg(db.QueryRowContext(ctx, `SELECT `+msgColumns+` FROM dc_messages WHERE chat = ? AND id = ?`, chat, id))
	if err != nil {
		return nil, err
	}
	m.Reactions = db.reactions(ctx, chat, id)
	return m, nil
}

func (db DB) setText(ctx context.Context, chat, id, text string) error {
	_, err := db.ExecContext(ctx, `UPDATE dc_messages SET text = ?, edited = 1 WHERE chat = ? AND id = ?`, text, chat, id)
	return err
}

func (db DB) setDeleted(ctx context.Context, chat, id string) error {
	_, err := db.ExecContext(ctx, `UPDATE dc_messages SET deleted = 1, text = '', file = '', raw = NULL WHERE chat = ? AND id = ?`, chat, id)
	return err
}

func (db DB) setReaction(ctx context.Context, chat, id, sender, emoji string) error {
	if emoji == "" {
		_, err := db.ExecContext(ctx, `DELETE FROM dc_reactions WHERE chat = ? AND msg_id = ? AND sender = ?`, chat, id, sender)
		return err
	}
	_, err := db.ExecContext(ctx, `
		INSERT INTO dc_reactions (chat, msg_id, sender, emoji) VALUES (?, ?, ?, ?)
		ON CONFLICT (chat, msg_id, sender) DO UPDATE SET emoji = excluded.emoji`, chat, id, sender, emoji)
	return err
}

// reactionBy returns the emoji one of the given senders reacted with, if any.
func (db DB) reactionBy(ctx context.Context, chat, id string, senders []string) string {
	if len(senders) == 0 {
		return ""
	}
	q := `SELECT emoji FROM dc_reactions WHERE chat = ? AND msg_id = ? AND sender IN (?` + strings.Repeat(",?", len(senders)-1) + `) LIMIT 1`
	args := []any{chat, id}
	for _, s := range senders {
		args = append(args, s)
	}
	var emoji string
	_ = db.QueryRowContext(ctx, q, args...).Scan(&emoji)
	return emoji
}

func (db DB) reactions(ctx context.Context, chat, id string) string {
	rows, err := db.QueryContext(ctx, `SELECT emoji, COUNT(*) FROM dc_reactions WHERE chat = ? AND msg_id = ? GROUP BY emoji ORDER BY COUNT(*) DESC`, chat, id)
	if err != nil {
		return ""
	}
	defer rows.Close()
	var parts []string
	for rows.Next() {
		var emoji string
		var n int
		if rows.Scan(&emoji, &n) == nil {
			if n > 1 {
				emoji += " " + strconv.Itoa(n)
			}
			parts = append(parts, emoji)
		}
	}
	return strings.Join(parts, "  ")
}

// history returns up to limit messages older than (beforeTS, beforeID), oldest first.
func (db DB) history(ctx context.Context, chat string, beforeTS int64, beforeID string, limit int) ([]*Msg, error) {
	query := `SELECT ` + msgColumns + ` FROM dc_messages WHERE chat = ?`
	args := []any{chat}
	if beforeTS > 0 {
		query += ` AND (ts < ? OR (ts = ? AND id < ?))`
		args = append(args, beforeTS, beforeTS, beforeID)
	}
	query += ` ORDER BY ts DESC, id DESC LIMIT ?`
	args = append(args, limit)
	rows, err := db.QueryContext(ctx, query, args...)
	if err != nil {
		return nil, err
	}
	var msgs []*Msg
	for rows.Next() {
		m, err := scanMsg(rows)
		if err != nil {
			rows.Close()
			return nil, err
		}
		msgs = append(msgs, m)
	}
	rows.Close()
	for i, j := 0, len(msgs)-1; i < j; i, j = i+1, j-1 {
		msgs[i], msgs[j] = msgs[j], msgs[i]
	}
	for _, m := range msgs {
		m.Reactions = db.reactions(ctx, chat, m.ID)
	}
	return msgs, nil
}

// touchChat creates the chat if needed and bumps its last activity.
func (db DB) touchChat(ctx context.Context, jid string, isGroup bool, ts int64, fromMe bool) error {
	readTS := int64(0)
	if fromMe {
		readTS = ts
	}
	_, err := db.ExecContext(ctx, `
		INSERT INTO dc_chats (jid, is_group, last_ts, read_ts) VALUES (?, ?, ?, ?)
		ON CONFLICT (jid) DO UPDATE SET
			last_ts = MAX(last_ts, excluded.last_ts),
			read_ts = MAX(read_ts, excluded.read_ts)`, jid, isGroup, ts, readTS)
	return err
}

// setPinned records when a chat was pinned (0: unpinned).
func (db DB) setPinned(ctx context.Context, jid string, isGroup bool, pinned int64) error {
	_, err := db.ExecContext(ctx, `
		INSERT INTO dc_chats (jid, is_group, pinned) VALUES (?, ?, ?)
		ON CONFLICT (jid) DO UPDATE SET pinned = excluded.pinned`, jid, isGroup, pinned)
	return err
}

func (db DB) setChatName(ctx context.Context, jid, name string) error {
	_, err := db.ExecContext(ctx, `UPDATE dc_chats SET name = ? WHERE jid = ?`, name, jid)
	return err
}

func (db DB) chatName(ctx context.Context, jid string) string {
	var name string
	_ = db.QueryRowContext(ctx, `SELECT name FROM dc_chats WHERE jid = ?`, jid).Scan(&name)
	return name
}

// markRead sets the read marker to the newest message and returns the
// previous marker.
func (db DB) markRead(ctx context.Context, jid string) (int64, error) {
	var prev int64
	if err := db.QueryRowContext(ctx, `SELECT read_ts FROM dc_chats WHERE jid = ?`, jid).Scan(&prev); err != nil {
		return 0, err
	}
	_, err := db.ExecContext(ctx, `UPDATE dc_chats SET read_ts = MAX(read_ts, last_ts) WHERE jid = ?`, jid)
	return prev, err
}

// setUnread places the read marker so that `unread` incoming messages remain unread.
func (db DB) setUnread(ctx context.Context, jid string, unread int) error {
	var ts int64
	err := db.QueryRowContext(ctx, `
		SELECT ts FROM dc_messages WHERE chat = ? AND from_me = 0 ORDER BY ts DESC LIMIT 1 OFFSET ?`, jid, unread).Scan(&ts)
	if err == sql.ErrNoRows {
		ts = 0
	} else if err != nil {
		return err
	}
	if unread == 0 {
		_, err = db.ExecContext(ctx, `UPDATE dc_chats SET read_ts = MAX(read_ts, last_ts) WHERE jid = ?`, jid)
	} else {
		_, err = db.ExecContext(ctx, `UPDATE dc_chats SET read_ts = ? WHERE jid = ?`, ts, jid)
	}
	return err
}

// unreadIncoming lists incoming message ids newer than ts, grouped by sender.
func (db DB) unreadIncoming(ctx context.Context, jid string, ts int64) (map[string][]string, error) {
	rows, err := db.QueryContext(ctx, `SELECT sender, id FROM dc_messages WHERE chat = ? AND from_me = 0 AND ts > ?`, jid, ts)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string][]string{}
	for rows.Next() {
		var sender, id string
		if rows.Scan(&sender, &id) == nil {
			out[sender] = append(out[sender], id)
		}
	}
	return out, nil
}

const chatQuery = `
	SELECT c.jid, c.name, c.is_group, c.last_ts,
		(SELECT COUNT(*) FROM dc_messages m WHERE m.chat = c.jid AND m.from_me = 0 AND m.ts > c.read_ts),
		c.pinned
	FROM dc_chats c`

func scanChat(row interface{ Scan(...any) error }) (*Chat, error) {
	var c Chat
	if err := row.Scan(&c.JID, &c.Name, &c.IsGroup, &c.LastTS, &c.Unread, &c.Pinned); err != nil {
		return nil, err
	}
	return &c, nil
}

func (db DB) chat(ctx context.Context, jid string) (*Chat, error) {
	return scanChat(db.QueryRowContext(ctx, chatQuery+` WHERE c.jid = ?`, jid))
}

func (db DB) chats(ctx context.Context) ([]*Chat, error) {
	rows, err := db.QueryContext(ctx, chatQuery+` WHERE c.last_ts > 0 OR c.pinned > 0 ORDER BY c.last_ts DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Chat
	for rows.Next() {
		c, err := scanChat(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, c)
	}
	return out, nil
}
