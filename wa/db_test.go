package main

import (
	"context"
	"database/sql"
	"testing"

	"go.mau.fi/whatsmeow/proto/waE2E"
	"google.golang.org/protobuf/proto"
)

func testDB(t *testing.T) DB {
	t.Helper()
	raw, err := sql.Open("sqlite", "file:"+t.TempDir()+"/t.db")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { raw.Close() })
	db := DB{raw}
	if err := db.init(context.Background()); err != nil {
		t.Fatal(err)
	}
	return db
}

func TestHistoryAndUnread(t *testing.T) {
	ctx := context.Background()
	db := testDB(t)
	chat := "1@s.whatsapp.net"
	for i, id := range []string{"a", "b", "c", "d"} {
		fromMe := id == "b"
		if _, err := db.insertMsg(ctx, &Msg{ID: id, Chat: chat, Sender: chat, FromMe: fromMe, TS: int64(10 + i), Kind: "text", Text: id}); err != nil {
			t.Fatal(err)
		}
		if err := db.touchChat(ctx, chat, false, int64(10+i), fromMe); err != nil {
			t.Fatal(err)
		}
	}
	// Duplicate inserts are ignored.
	if ok, _ := db.insertMsg(ctx, &Msg{ID: "a", Chat: chat, Kind: "text"}); ok {
		t.Fatal("duplicate inserted")
	}

	page, err := db.history(ctx, chat, 0, "", 2)
	if err != nil || len(page) != 2 || page[0].ID != "c" || page[1].ID != "d" {
		t.Fatalf("latest page: %v %+v", err, page)
	}
	older, _ := db.history(ctx, chat, page[0].TS, page[0].ID, 50)
	if len(older) != 2 || older[0].ID != "a" || older[1].ID != "b" {
		t.Fatalf("older page: %+v", older)
	}

	// Own message "b" moved the read marker; "c" and "d" are unread.
	c, _ := db.chat(ctx, chat)
	if c.Unread != 2 {
		t.Fatalf("unread = %d, want 2", c.Unread)
	}
	pending, _ := db.unreadIncoming(ctx, chat, 11)
	if len(pending[chat]) != 2 {
		t.Fatalf("pending = %v", pending)
	}
	if _, err := db.markRead(ctx, chat); err != nil {
		t.Fatal(err)
	}
	if c, _ = db.chat(ctx, chat); c.Unread != 0 {
		t.Fatalf("unread after markRead = %d", c.Unread)
	}
	if err := db.setUnread(ctx, chat, 1); err != nil {
		t.Fatal(err)
	}
	if c, _ = db.chat(ctx, chat); c.Unread != 1 {
		t.Fatalf("unread after setUnread(1) = %d", c.Unread)
	}
}

func TestInitIsIdempotent(t *testing.T) {
	ctx := context.Background()
	db := testDB(t)
	if err := db.init(ctx); err != nil {
		t.Fatal(err)
	}
	_, _ = db.insertMsg(ctx, &Msg{ID: "r", Chat: "c", Kind: "text", ReplyID: "q", ReplySender: "2@s.whatsapp.net"})
	if m, err := db.getMsg(ctx, "c", "r"); err != nil || m.ReplySender != "2@s.whatsapp.net" {
		t.Fatalf("%v %+v", err, m)
	}
}

func TestEditsDeletesReactions(t *testing.T) {
	ctx := context.Background()
	db := testDB(t)
	chat := "g@g.us"
	_, _ = db.insertMsg(ctx, &Msg{ID: "x", Chat: chat, Kind: "image", Text: "hi", raw: []byte{1}})
	_ = db.setText(ctx, chat, "x", "edited")
	_ = db.setReaction(ctx, chat, "x", "a", "👍")
	_ = db.setReaction(ctx, chat, "x", "b", "👍")
	m, err := db.getMsg(ctx, chat, "x")
	if err != nil || m.Text != "edited" || !m.Edited || !m.Media || m.Reactions != "👍 2" {
		t.Fatalf("%v %+v", err, m)
	}
	if got := db.reactionBy(ctx, chat, "x", []string{"z", "b"}); got != "👍" {
		t.Fatalf("reactionBy = %q", got)
	}
	if got := db.reactionBy(ctx, chat, "x", []string{"z"}); got != "" {
		t.Fatalf("reactionBy(other) = %q", got)
	}
	_ = db.setDeleted(ctx, chat, "x")
	if m, _ = db.getMsg(ctx, chat, "x"); !m.Deleted || m.Media || m.Text != "" {
		t.Fatalf("after delete: %+v", m)
	}
}

func TestSafeName(t *testing.T) {
	for in, want := range map[string]string{
		"../../evil.exe":   "evil.exe",
		`..\..\x.bat`:      "x.bat",
		"a:stream.txt":     "a_stream.txt",
		"inv\u202Efdp.exe": "inv_fdp.exe",
		"...":              "file",
		"photo.jpg":        "photo.jpg",
	} {
		if got := safeName(in); got != want {
			t.Errorf("safeName(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestExtract(t *testing.T) {
	c := extract(&waE2E.Message{AudioMessage: &waE2E.AudioMessage{PTT: proto.Bool(true), Seconds: proto.Uint32(75)}})
	if c.kind != "voice" || c.text != "1:15" || !c.media {
		t.Fatalf("%+v", c)
	}
	if extract(&waE2E.Message{}).kind != "" {
		t.Fatal("empty message should be skipped")
	}
	if p := preview(&waE2E.Message{DocumentMessage: &waE2E.DocumentMessage{FileName: proto.String("a.pdf")}}); p != "[document] a.pdf" {
		t.Fatalf("preview %q", p)
	}
}
