// discommand-wa is the WhatsApp backend of discommand. It links to a
// WhatsApp account as a companion device (whatsmeow) and talks to the
// terminal UI through JSON lines: events on stdout, commands on stdin.
package main

import (
	"bufio"
	"context"
	"database/sql"
	"encoding/json"
	"flag"
	"fmt"
	"mime"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/appstate"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/protobuf/proto"
	_ "modernc.org/sqlite"
)

type App struct {
	cli      *whatsmeow.Client
	db       DB
	cacheDir string

	outMu sync.Mutex
	out   *json.Encoder

	// Debounces name refreshes after contact syncs.
	namesMu    sync.Mutex
	namesTimer *time.Timer
}

type event map[string]any

func (a *App) emit(e event) {
	a.outMu.Lock()
	defer a.outMu.Unlock()
	_ = a.out.Encode(e)
}

func (a *App) fail(format string, args ...any) {
	a.emit(event{"t": "error", "msg": fmt.Sprintf(format, args...)})
}

func main() {
	dbPath := flag.String("db", "whatsapp.db", "database path")
	cacheDir := flag.String("cache", os.TempDir(), "directory for downloaded media")
	logPath := flag.String("log", "", "write debug logs to this file")
	flag.Parse()

	if err := run(*dbPath, *cacheDir, *logPath); err != nil {
		_ = json.NewEncoder(os.Stdout).Encode(event{"t": "fatal", "msg": err.Error()})
		os.Exit(1)
	}
}

func run(dbPath, cacheDir, logPath string) error {
	ctx := context.Background()
	log := waLog.Noop
	if logPath != "" {
		f, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
		if err != nil {
			return err
		}
		log = fileLogger{f: f, module: "wa"}
	}

	if err := os.MkdirAll(filepath.Dir(dbPath), 0o700); err != nil {
		return err
	}
	dsn := "file:" + filepath.ToSlash(dbPath) + "?_pragma=foreign_keys(1)&_pragma=journal_mode(WAL)&_pragma=busy_timeout(10000)"
	rawDB, err := sql.Open("sqlite", dsn)
	if err != nil {
		return err
	}
	container := sqlstore.NewWithDB(rawDB, "sqlite", log.Sub("store"))
	if err := container.Upgrade(ctx); err != nil {
		return fmt.Errorf("upgrade store: %w", err)
	}
	db := DB{rawDB}
	if err := db.init(ctx); err != nil {
		return fmt.Errorf("init tables: %w", err)
	}
	device, err := container.GetFirstDevice(ctx)
	if err != nil {
		return err
	}
	store.SetOSInfo("discommand", [3]uint32{0, 1, 0})

	a := &App{db: db, cacheDir: cacheDir, out: json.NewEncoder(os.Stdout)}
	a.cli = whatsmeow.NewClient(device, log.Sub("client"))
	// Chat settings (pins) also arrive when the whole app state is synced.
	a.cli.EmitAppStateEventsOnFullSync = true
	a.cli.AddEventHandler(a.onEvent)

	if a.cli.Store.ID == nil {
		qr, err := a.cli.GetQRChannel(ctx)
		if err != nil {
			return err
		}
		a.emit(event{"t": "login"})
		go func() {
			for item := range qr {
				switch item.Event {
				case "code":
					a.emit(event{"t": "qr", "code": item.Code})
				case "success":
					// PairSuccess / Connected events report the rest.
				default:
					a.emit(event{"t": "fatal", "msg": "linking failed: " + item.Event})
					os.Exit(1)
				}
			}
		}()
		if err := a.cli.Connect(); err != nil {
			return err
		}
	} else {
		a.emitLinked(ctx)
		go func() {
			if err := a.cli.Connect(); err != nil {
				a.emit(event{"t": "status", "status": "offline: " + err.Error()})
			}
		}()
	}

	go cleanCache(cacheDir)
	a.readCommands(ctx)
	a.cli.Disconnect()
	return nil
}

func (a *App) emitLinked(ctx context.Context) {
	me := a.cli.Store.ID.ToNonAD().String()
	a.emit(event{"t": "linked", "me": me, "name": a.cli.Store.PushName, "receipts": a.receipts(ctx)})
	a.emitChats(ctx)
}

func (a *App) emitChats(ctx context.Context) {
	chats, err := a.db.chats(ctx)
	if err != nil {
		a.fail("load chats: %v", err)
		return
	}
	shown := []*Chat{}
	for _, c := range chats {
		// Stored before such chats were filtered out.
		if j, err := types.ParseJID(c.JID); err == nil && hiddenChat(j) {
			continue
		}
		a.resolveChat(ctx, c)
		shown = append(shown, c)
	}
	chats = shown
	a.emit(event{"t": "chats", "chats": chats})
}

// hiddenChat tells chats that aren't conversations here: status updates and
// broadcast lists, channels, and WhatsApp's announcement account
// (0@s.whatsapp.net, which would show as a nameless "+0").
func hiddenChat(j types.JID) bool {
	return j.Server == types.BroadcastServer || j.Server == types.NewsletterServer ||
		j.ToNonAD() == types.PSAJID || j.ToNonAD() == types.LegacyPSAJID
}

func (a *App) emitChat(ctx context.Context, jid string) {
	if c, err := a.db.chat(ctx, jid); err == nil {
		a.resolveChat(ctx, c)
		a.emit(event{"t": "chat", "chat": c})
	}
}

// resolveChat names direct chats from the current contact list; the stored
// name is only a fallback (contacts usually sync after the first messages).
func (a *App) resolveChat(ctx context.Context, c *Chat) {
	if c.IsGroup {
		return
	}
	if j, err := types.ParseJID(c.JID); err == nil {
		c.Name = a.userName(ctx, j, c.Name)
	}
}

// resolveMsg refreshes sender and quoted-sender names from the contact list
// and fills in picture dimensions.
func (a *App) resolveMsg(ctx context.Context, m *Msg) {
	if m.Media {
		var msg waE2E.Message
		if proto.Unmarshal(m.raw, &msg) == nil {
			m.ImgW, m.ImgH = dimensions(&msg)
		}
	}
	// One sender id per person (the phone-number form), so the UI groups
	// and colors their messages consistently. Older rows may hold either form.
	if m.FromMe {
		if a.cli.Store.ID != nil {
			m.Sender = a.cli.Store.ID.ToNonAD().String()
		}
		if a.cli.Store.PushName != "" {
			m.SenderName = a.cli.Store.PushName
		}
	} else if j, err := types.ParseJID(m.Sender); err == nil {
		j = a.canonical(ctx, j)
		m.Sender = j.String()
		m.SenderName = a.userName(ctx, j, m.SenderName)
	}
	if m.ReplySender != "" {
		if j, err := types.ParseJID(m.ReplySender); err == nil {
			if a.isMe(ctx, j) {
				m.ReplyName = a.cli.Store.PushName
			} else {
				m.ReplyName = a.userName(ctx, j, m.ReplyName)
			}
		}
	}
}

// namesChanged schedules one chat-list refresh (and a reload hint for the UI)
// once a burst of contact/push-name updates has settled.
func (a *App) namesChanged() {
	a.namesMu.Lock()
	defer a.namesMu.Unlock()
	if a.namesTimer != nil {
		a.namesTimer.Stop()
	}
	a.namesTimer = time.AfterFunc(time.Second, func() {
		ctx := context.Background()
		a.emitChats(ctx)
		a.emit(event{"t": "names"})
	})
}

func (a *App) receipts(ctx context.Context) bool {
	return a.db.kv(ctx, "read_receipts") == "on"
}

// ---- names and addressing ---------------------------------------------

// canonical maps hidden-user (LID) JIDs to phone-number JIDs so that one
// person never shows up as two chats.
func (a *App) canonical(ctx context.Context, j types.JID) types.JID {
	j = j.ToNonAD()
	if j.Server == types.HiddenUserServer {
		if pn, err := a.cli.Store.LIDs.GetPNForLID(ctx, j); err == nil && !pn.IsEmpty() {
			return pn.ToNonAD()
		}
	}
	return j
}

// isMe reports whether a JID (phone number or LID form) is our own account.
func (a *App) isMe(ctx context.Context, j types.JID) bool {
	id := a.cli.Store.ID
	return id != nil && a.canonical(ctx, j).User == id.User
}

// userName resolves a display name the way the phone does: an address-book
// name first, then the person's own WhatsApp name, then the number. A person
// can have contact records under both their phone-number JID and their
// hidden-user (LID) JID, so both are consulted; the address-book name
// usually sits on the phone-number one.
func (a *App) userName(ctx context.Context, j types.JID, fallback string) string {
	j = j.ToNonAD()
	pn := a.canonical(ctx, j)
	ids := []types.JID{pn}
	if pn != j {
		ids = append(ids, j)
	} else if pn.Server == types.DefaultUserServer {
		if lid, err := a.cli.Store.LIDs.GetLIDForPN(ctx, pn); err == nil && !lid.IsEmpty() {
			ids = append(ids, lid.ToNonAD())
		}
	}
	push := ""
	for _, id := range ids {
		c, err := a.cli.Store.Contacts.GetContact(ctx, id)
		if err != nil || !c.Found {
			continue
		}
		for _, n := range []string{c.FullName, c.FirstName, c.BusinessName} {
			if n != "" {
				return n
			}
		}
		if push == "" {
			push = c.PushName
		}
	}
	switch {
	case push != "":
		return push
	case fallback != "":
		return fallback
	case pn.Server == types.DefaultUserServer:
		return "+" + pn.User
	}
	return pn.User
}

// ensureChatName fills in a chat's display name if it has none yet.
func (a *App) ensureChatName(ctx context.Context, chat types.JID, hint string) {
	key := chat.String()
	if a.db.chatName(ctx, key) != "" {
		return
	}
	name := hint
	if name == "" && chat.Server == types.GroupServer {
		if info, err := a.cli.GetGroupInfo(ctx, chat); err == nil {
			name = info.Name
		}
	}
	if name == "" && chat.Server != types.GroupServer {
		name = a.userName(ctx, chat, "")
	}
	if name != "" {
		_ = a.db.setChatName(ctx, key, name)
	}
}

// resolveMentions replaces "@<number>" with "@<name>".
func (a *App) resolveMentions(ctx context.Context, text string, ci *waE2E.ContextInfo) string {
	for _, raw := range ci.GetMentionedJID() {
		j, err := types.ParseJID(raw)
		if err != nil {
			continue
		}
		text = strings.ReplaceAll(text, "@"+j.User, "@"+a.userName(ctx, j, ""))
	}
	return text
}

// ---- incoming events ----------------------------------------------------

func (a *App) onEvent(raw any) {
	ctx := context.Background()
	switch evt := raw.(type) {
	case *events.Message:
		a.onMessage(ctx, evt, true)
	case *events.HistorySync:
		a.onHistorySync(ctx, evt)
	case *events.PairSuccess:
		a.emit(event{"t": "paired"})
		a.emitLinked(ctx)
	case *events.Connected:
		// Pins weren't read before this version: fetch the chat settings once
		// more (a download only; nothing changes on the phone).
		if a.cli.Store.ID != nil && a.db.kv(ctx, "pins_synced") == "" {
			go func() {
				if err := a.cli.FetchAppState(ctx, appstate.WAPatchRegularLow, true, false); err == nil {
					_ = a.db.setKV(ctx, "pins_synced", "1")
				}
			}()
		}
		if a.cli.Store.ID != nil {
			a.emit(event{"t": "status", "status": "connected"})
		}
	case *events.Disconnected:
		a.emit(event{"t": "status", "status": "reconnecting…"})
	case *events.KeepAliveTimeout:
		a.emit(event{"t": "status", "status": "connection unstable"})
	case *events.KeepAliveRestored:
		a.emit(event{"t": "status", "status": "connected"})
	case *events.LoggedOut:
		a.emit(event{"t": "logged_out", "msg": "this device was unlinked from WhatsApp"})
	case *events.StreamReplaced:
		a.emit(event{"t": "fatal", "msg": "WhatsApp session was taken over by another discommand instance"})
		os.Exit(1)
	case *events.TemporaryBan:
		a.emit(event{"t": "fatal", "msg": "WhatsApp temporary ban: " + evt.String()})
		os.Exit(1)
	case *events.ClientOutdated:
		a.emit(event{"t": "fatal", "msg": "WhatsApp rejected this client version; rebuild discommand-wa with an updated whatsmeow"})
		os.Exit(1)
	case *events.GroupInfo:
		if evt.Name != nil {
			jid := evt.JID.String()
			_ = a.db.setChatName(ctx, jid, evt.Name.Name)
			a.emitChat(ctx, jid)
		}
	case *events.JoinedGroup:
		jid := evt.JID.String()
		_ = a.db.touchChat(ctx, jid, true, time.Now().Unix(), false)
		_ = a.db.setChatName(ctx, jid, evt.Name)
		a.emitChat(ctx, jid)
	case *events.AppStateSyncComplete, *events.Contact, *events.PushName, *events.BusinessName:
		a.namesChanged()
	case *events.Pin:
		// Pinned or unpinned on the phone (or another device).
		jid := a.canonical(ctx, evt.JID)
		var at int64
		if evt.Action.GetPinned() {
			at = evt.Timestamp.Unix()
			if at <= 0 {
				at = 1
			}
		}
		_ = a.db.setPinned(ctx, jid.String(), jid.Server == types.GroupServer, at)
		a.emitChat(ctx, jid.String())
	case *events.MarkChatAsRead:
		// Read on another device (e.g. the phone).
		jid := a.canonical(ctx, evt.JID).String()
		if evt.Action.GetRead() {
			_, _ = a.db.markRead(ctx, jid)
			a.emitChat(ctx, jid)
		}
	}
}

func (a *App) onMessage(ctx context.Context, evt *events.Message, live bool) {
	info := evt.Info
	if hiddenChat(info.Chat) {
		return
	}
	chatJID := a.canonical(ctx, info.Chat)
	chat := chatJID.String()
	m := evt.Message

	if pm := m.GetProtocolMessage(); pm != nil {
		target := pm.GetKey().GetID()
		switch pm.GetType() {
		case waE2E.ProtocolMessage_REVOKE:
			_ = a.db.setDeleted(ctx, chat, target)
		case waE2E.ProtocolMessage_MESSAGE_EDIT:
			c := extract(pm.GetEditedMessage())
			_ = a.db.setText(ctx, chat, target, a.resolveMentions(ctx, c.text, c.ctx))
		default:
			return
		}
		if live {
			a.emitMsg(ctx, chat, target)
		}
		return
	}
	if r := m.GetReactionMessage(); r != nil {
		target := r.GetKey().GetID()
		_ = a.db.setReaction(ctx, chat, target, a.canonical(ctx, info.Sender).String(), r.GetText())
		if live {
			a.emitMsg(ctx, chat, target)
		}
		return
	}
	if evt.IsEdit {
		c := extract(m)
		_ = a.db.setText(ctx, chat, info.ID, a.resolveMentions(ctx, c.text, c.ctx))
		if live {
			a.emitMsg(ctx, chat, info.ID)
		}
		return
	}

	c := extract(m)
	if c.kind == "" {
		return
	}
	msg := &Msg{
		ID:     info.ID,
		Chat:   chat,
		Sender: info.Sender.ToNonAD().String(),
		FromMe: info.IsFromMe,
		TS:     info.Timestamp.Unix(),
		Kind:   c.kind,
		Text:   a.resolveMentions(ctx, c.text, c.ctx),
		File:   c.file,
		Media:  c.media,
	}
	if info.IsFromMe {
		msg.SenderName = a.cli.Store.PushName
	} else {
		msg.SenderName = a.userName(ctx, info.Sender, info.PushName)
	}
	if c.ctx.GetStanzaID() != "" {
		msg.ReplyID = c.ctx.GetStanzaID()
		msg.ReplyText = preview(c.ctx.GetQuotedMessage())
		if p, err := types.ParseJID(c.ctx.GetParticipant()); err == nil && !p.IsEmpty() {
			msg.ReplySender = p.ToNonAD().String()
			if a.isMe(ctx, p) {
				msg.ReplyName = a.cli.Store.PushName
			} else {
				msg.ReplyName = a.userName(ctx, p, "")
			}
		}
	}
	if c.media {
		msg.raw, _ = proto.Marshal(m)
	}
	inserted, err := a.db.insertMsg(ctx, msg)
	if err != nil {
		a.fail("store message: %v", err)
		return
	}
	_ = a.db.touchChat(ctx, chat, info.IsGroup, msg.TS, info.IsFromMe)
	if !live {
		return
	}
	a.ensureChatName(ctx, chatJID, "")
	if inserted {
		a.resolveMsg(ctx, msg)
		a.emit(event{"t": "msg", "msg": msg})
	}
	a.emitChat(ctx, chat)
}

func (a *App) emitMsg(ctx context.Context, chat, id string) {
	if m, err := a.db.getMsg(ctx, chat, id); err == nil {
		a.resolveMsg(ctx, m)
		a.emit(event{"t": "msg", "msg": m})
	}
}

func (a *App) onHistorySync(ctx context.Context, evt *events.HistorySync) {
	for _, conv := range evt.Data.GetConversations() {
		jid, err := types.ParseJID(conv.GetID())
		if err != nil || hiddenChat(jid) {
			continue
		}
		for _, hm := range conv.GetMessages() {
			parsed, err := a.cli.ParseWebMessage(jid, hm.GetMessage())
			if err == nil {
				a.onMessage(ctx, parsed, false)
			}
		}
		chatJID := a.canonical(ctx, jid)
		chat := chatJID.String()
		if ts := int64(conv.GetConversationTimestamp()); ts > 0 {
			_ = a.db.touchChat(ctx, chat, jid.Server == types.GroupServer, ts, false)
		}
		if conv.GetName() != "" {
			_ = a.db.setChatName(ctx, chat, conv.GetName())
		} else if chatJID.Server != types.GroupServer {
			a.ensureChatName(ctx, chatJID, "")
		}
		_ = a.db.setUnread(ctx, chat, int(conv.GetUnreadCount()))
		if p := conv.GetPinned(); p > 0 {
			_ = a.db.setPinned(ctx, chat, jid.Server == types.GroupServer, int64(p))
		}
	}
	a.emitChats(ctx)
}

// ---- commands -------------------------------------------------------------

type command struct {
	C        string `json:"c"`
	Chat     string `json:"chat"`
	ID       string `json:"id"`
	Text     string `json:"text"`
	ReplyTo  string `json:"reply_to"`
	BeforeTS int64  `json:"before_ts"`
	BeforeID string `json:"before_id"`
	Phone    string `json:"phone"`
	On       bool   `json:"on"`
	Key      string `json:"key"`
	Path     string `json:"path"`
	Full     bool   `json:"full"`
}

func (a *App) readCommands(ctx context.Context) {
	sc := bufio.NewScanner(os.Stdin)
	sc.Buffer(make([]byte, 64*1024), 16*1024*1024)
	for sc.Scan() {
		var cmd command
		if err := json.Unmarshal(sc.Bytes(), &cmd); err != nil {
			a.fail("bad command: %v", err)
			continue
		}
		go a.handle(ctx, cmd)
	}
}

func (a *App) handle(ctx context.Context, cmd command) {
	chat, _ := types.ParseJID(cmd.Chat)
	switch cmd.C {
	case "history":
		msgs, err := a.db.history(ctx, cmd.Chat, cmd.BeforeTS, cmd.BeforeID, 50)
		if err != nil {
			a.fail("load history: %v", err)
			return
		}
		if msgs == nil {
			msgs = []*Msg{}
		}
		for _, m := range msgs {
			a.resolveMsg(ctx, m)
		}
		a.emit(event{"t": "history", "chat": cmd.Chat, "older": cmd.BeforeTS > 0, "msgs": msgs, "complete": len(msgs) < 50})
	case "send":
		a.send(ctx, chat, cmd.Text, cmd.ReplyTo)
	case "file":
		a.sendFile(ctx, chat, cmd.Path, cmd.Text, cmd.ReplyTo)
	case "edit":
		edit := a.cli.BuildEdit(chat, cmd.ID, &waE2E.Message{Conversation: proto.String(cmd.Text)})
		if _, err := a.cli.SendMessage(ctx, chat, edit); err != nil {
			a.fail("edit failed: %v", err)
			return
		}
		_ = a.db.setText(ctx, cmd.Chat, cmd.ID, cmd.Text)
		a.emitMsg(ctx, cmd.Chat, cmd.ID)
	case "delete":
		m, err := a.db.getMsg(ctx, cmd.Chat, cmd.ID)
		if err != nil {
			a.fail("delete failed: message not found")
			return
		}
		sender := types.EmptyJID
		if !m.FromMe {
			// Admins can delete others' messages in groups.
			sender, _ = types.ParseJID(m.Sender)
		}
		if _, err := a.cli.SendMessage(ctx, chat, a.cli.BuildRevoke(chat, sender, cmd.ID)); err != nil {
			a.fail("delete failed: %v", err)
			return
		}
		_ = a.db.setDeleted(ctx, cmd.Chat, cmd.ID)
		a.emitMsg(ctx, cmd.Chat, cmd.ID)
	case "reconnect":
		if !a.cli.IsConnected() {
			a.cli.Disconnect()
			if err := a.cli.Connect(); err != nil {
				a.fail("reconnect failed: %v", err)
			}
		}
	case "react":
		a.react(ctx, chat, cmd.ID, cmd.Text)
	case "seen":
		a.seen(ctx, chat)
	case "receipts":
		v := "off"
		if cmd.On {
			v = "on"
		}
		_ = a.db.setKV(ctx, "read_receipts", v)
		a.emit(event{"t": "receipts", "on": cmd.On})
	case "thumb":
		a.emit(event{"t": "thumb", "key": cmd.Key, "data": a.thumbnail(ctx, cmd.Chat, cmd.ID, cmd.Full)})
	case "download":
		a.download(ctx, cmd.Chat, cmd.ID)
	case "pair":
		code, err := a.cli.PairPhone(ctx, cmd.Phone, true, whatsmeow.PairClientChrome, "Chrome (Linux)")
		if err != nil {
			a.fail("pairing failed: %v", err)
			return
		}
		a.emit(event{"t": "pair_code", "code": code})
	case "logout":
		if err := a.cli.Logout(ctx); err != nil {
			a.fail("logout failed: %v", err)
			return
		}
		a.emit(event{"t": "logged_out", "msg": "logged out of WhatsApp"})
	default:
		a.fail("unknown command %q", cmd.C)
	}
}

func (a *App) send(ctx context.Context, chat types.JID, text, replyTo string) {
	msg := &waE2E.Message{Conversation: proto.String(text)}
	var quoted *Msg
	if replyTo != "" {
		if q, err := a.db.getMsg(ctx, chat.String(), replyTo); err == nil {
			quoted = q
			participant := q.Sender
			if q.FromMe && a.cli.Store.ID != nil {
				participant = a.cli.Store.ID.ToNonAD().String()
			}
			msg = &waE2E.Message{ExtendedTextMessage: &waE2E.ExtendedTextMessage{
				Text: proto.String(text),
				ContextInfo: &waE2E.ContextInfo{
					StanzaID:      proto.String(q.ID),
					Participant:   proto.String(participant),
					QuotedMessage: &waE2E.Message{Conversation: proto.String(q.Text)},
				},
			}}
		}
	}
	resp, err := a.cli.SendMessage(ctx, chat, msg)
	if err != nil {
		a.fail("send failed: %v", err)
		return
	}
	own := &Msg{
		ID:         resp.ID,
		Chat:       chat.String(),
		Sender:     a.cli.Store.ID.ToNonAD().String(),
		SenderName: a.cli.Store.PushName,
		FromMe:     true,
		TS:         resp.Timestamp.Unix(),
		Kind:       "text",
		Text:       text,
	}
	if quoted != nil {
		own.ReplyID, own.ReplyName, own.ReplyText = quoted.ID, quoted.SenderName, strings.ReplaceAll(quoted.Text, "\n", " ")
		own.ReplySender = quoted.Sender
	}
	_, _ = a.db.insertMsg(ctx, own)
	_ = a.db.touchChat(ctx, own.Chat, chat.Server == types.GroupServer, own.TS, true)
	a.emit(event{"t": "msg", "msg": own})
	a.emitChat(ctx, own.Chat)
}

// maxUpload is the largest file sent (WhatsApp allows more for documents,
// but the whole file is held in memory here).
const maxUpload = 100 << 20

// sendFile uploads a file and sends it as a photo, video or audio message
// by its type, anything else as a document; text is the caption.
func (a *App) sendFile(ctx context.Context, chat types.JID, path, caption, replyTo string) {
	info, err := os.Stat(path)
	if err != nil || info.IsDir() {
		a.fail("can't read %s", filepath.Base(path))
		return
	}
	if info.Size() > maxUpload {
		a.fail("%s is too large to send from here (over 100 MB)", filepath.Base(path))
		return
	}
	data, err := os.ReadFile(path)
	if err != nil {
		a.fail("can't read %s: %v", filepath.Base(path), err)
		return
	}
	name := filepath.Base(path)
	mimeType := mime.TypeByExtension(strings.ToLower(filepath.Ext(name)))
	if mimeType == "" {
		mimeType = http.DetectContentType(data)
	}
	mimeType, _, _ = strings.Cut(mimeType, ";")

	var replyCtx *waE2E.ContextInfo
	var quoted *Msg
	if replyTo != "" {
		if q, err := a.db.getMsg(ctx, chat.String(), replyTo); err == nil {
			quoted = q
			participant := q.Sender
			if q.FromMe && a.cli.Store.ID != nil {
				participant = a.cli.Store.ID.ToNonAD().String()
			}
			replyCtx = &waE2E.ContextInfo{
				StanzaID:      proto.String(q.ID),
				Participant:   proto.String(participant),
				QuotedMessage: &waE2E.Message{Conversation: proto.String(q.Text)},
			}
		}
	}

	kind := whatsmeow.MediaDocument
	switch {
	case mimeType == "image/jpeg" || mimeType == "image/png":
		kind = whatsmeow.MediaImage
	case mimeType == "video/mp4" || mimeType == "video/3gpp":
		kind = whatsmeow.MediaVideo
	case strings.HasPrefix(mimeType, "audio/"):
		kind = whatsmeow.MediaAudio
	}
	a.emit(event{"t": "status", "status": "sending " + name + "…"})
	up, err := a.cli.Upload(ctx, data, kind)
	if err != nil {
		a.emit(event{"t": "status", "status": "connected"})
		a.fail("upload failed: %v", err)
		return
	}
	var caption2 *string
	if caption != "" {
		caption2 = proto.String(caption)
	}
	msg := &waE2E.Message{}
	switch kind {
	case whatsmeow.MediaImage:
		msg.ImageMessage = &waE2E.ImageMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey,
			FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
			Mimetype: proto.String(mimeType), Caption: caption2, ContextInfo: replyCtx,
		}
	case whatsmeow.MediaVideo:
		msg.VideoMessage = &waE2E.VideoMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey,
			FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
			Mimetype: proto.String(mimeType), Caption: caption2, ContextInfo: replyCtx,
		}
	case whatsmeow.MediaAudio:
		msg.AudioMessage = &waE2E.AudioMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey,
			FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
			Mimetype: proto.String(mimeType), ContextInfo: replyCtx,
		}
	default:
		msg.DocumentMessage = &waE2E.DocumentMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey,
			FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
			Mimetype: proto.String(mimeType), FileName: proto.String(name), Title: proto.String(name),
			Caption: caption2, ContextInfo: replyCtx,
		}
	}
	resp, err := a.cli.SendMessage(ctx, chat, msg)
	a.emit(event{"t": "status", "status": "connected"})
	if err != nil {
		a.fail("send failed: %v", err)
		return
	}
	c := extract(msg)
	own := &Msg{
		ID:         resp.ID,
		Chat:       chat.String(),
		Sender:     a.cli.Store.ID.ToNonAD().String(),
		SenderName: a.cli.Store.PushName,
		FromMe:     true,
		TS:         resp.Timestamp.Unix(),
		Kind:       c.kind,
		Text:       c.text,
		File:       c.file,
		Media:      true,
	}
	own.raw, _ = proto.Marshal(msg)
	if quoted != nil {
		own.ReplyID, own.ReplyName, own.ReplyText = quoted.ID, quoted.SenderName, strings.ReplaceAll(quoted.Text, "\n", " ")
		own.ReplySender = quoted.Sender
	}
	_, _ = a.db.insertMsg(ctx, own)
	_ = a.db.touchChat(ctx, own.Chat, chat.Server == types.GroupServer, own.TS, true)
	a.emit(event{"t": "msg", "msg": own})
	a.emitChat(ctx, own.Chat)
}

// react toggles our reaction on a message: the same emoji again removes it,
// another one replaces it (WhatsApp allows one reaction per person).
func (a *App) react(ctx context.Context, chat types.JID, id, emoji string) {
	m, err := a.db.getMsg(ctx, chat.String(), id)
	if err != nil || a.cli.Store.ID == nil {
		a.fail("react: message not found")
		return
	}
	me := a.cli.Store.ID.ToNonAD().String()
	own := []string{me}
	if !a.cli.Store.LID.IsEmpty() {
		own = append(own, a.cli.Store.LID.ToNonAD().String())
	}
	if a.db.reactionBy(ctx, chat.String(), id, own) == emoji {
		emoji = ""
	}
	sender := types.EmptyJID
	if !m.FromMe {
		sender, _ = types.ParseJID(m.Sender)
	}
	if _, err := a.cli.SendMessage(ctx, chat, a.cli.BuildReaction(chat, sender, id, emoji)); err != nil {
		a.fail("reaction failed: %v", err)
		return
	}
	for _, o := range own {
		_ = a.db.setReaction(ctx, chat.String(), id, o, "")
	}
	_ = a.db.setReaction(ctx, chat.String(), id, me, emoji)
	a.emitMsg(ctx, chat.String(), id)
}

// seen marks a chat read locally; read receipts are only sent when enabled.
func (a *App) seen(ctx context.Context, chat types.JID) {
	key := chat.String()
	prev, err := a.db.markRead(ctx, key)
	if err != nil {
		return
	}
	if a.receipts(ctx) {
		pending, _ := a.db.unreadIncoming(ctx, key, prev)
		for sender, ids := range pending {
			s, _ := types.ParseJID(sender)
			if chat.Server != types.GroupServer {
				s = types.EmptyJID
			}
			_ = a.cli.MarkRead(ctx, ids, time.Now(), chat, s)
		}
	}
	a.emitChat(ctx, key)
}

func (a *App) download(ctx context.Context, chat, id string) {
	m, err := a.db.getMsg(ctx, chat, id)
	if err != nil || len(m.raw) == 0 {
		a.fail("no media in this message")
		return
	}
	var msg waE2E.Message
	if err := proto.Unmarshal(m.raw, &msg); err != nil {
		a.fail("download failed: %v", err)
		return
	}
	if n := fileLength(&msg); n > maxDownload {
		a.fail("file is too large to download here (%d MB)", n>>20)
		return
	}
	// Prefixed with the message id: names from other people never collide
	// with or overwrite other files. Pictures and voice notes have no name:
	// the id plus an extension, so the system knows how to open them.
	file := safeName(id) + extension(&msg)
	if m.File != "" {
		file = safeName(id) + "_" + safeName(m.File)
	}
	path := filepath.Join(a.cacheDir, file)
	if _, err := os.Stat(path); err != nil {
		a.emit(event{"t": "status", "status": "downloading…"})
		data, err := a.cli.DownloadAny(ctx, &msg)
		if err != nil {
			a.fail("download failed: %v", err)
			return
		}
		if err := os.MkdirAll(a.cacheDir, 0o700); err != nil {
			a.fail("download failed: %v", err)
			return
		}
		if err := os.WriteFile(path, data, 0o600); err != nil {
			a.fail("download failed: %v", err)
			return
		}
		a.emit(event{"t": "status", "status": "connected"})
	}
	a.emit(event{"t": "file", "path": path})
}

func dimensions(m *waE2E.Message) (uint32, uint32) {
	switch {
	case m.ImageMessage != nil:
		return m.GetImageMessage().GetWidth(), m.GetImageMessage().GetHeight()
	case m.VideoMessage != nil:
		return m.GetVideoMessage().GetWidth(), m.GetVideoMessage().GetHeight()
	case m.StickerMessage != nil:
		return m.GetStickerMessage().GetWidth(), m.GetStickerMessage().GetHeight()
	}
	return 0, 0
}

// maxDownload caps media downloads, which are held in memory.
const maxDownload = 256 << 20

func fileLength(m *waE2E.Message) uint64 {
	switch {
	case m.ImageMessage != nil:
		return m.GetImageMessage().GetFileLength()
	case m.VideoMessage != nil:
		return m.GetVideoMessage().GetFileLength()
	case m.AudioMessage != nil:
		return m.GetAudioMessage().GetFileLength()
	case m.DocumentMessage != nil:
		return m.GetDocumentMessage().GetFileLength()
	case m.StickerMessage != nil:
		return m.GetStickerMessage().GetFileLength()
	}
	return 0
}

// safeName turns a sender-chosen file name into a harmless one: no paths,
// no characters Windows treats specially, no alternate data streams.
func safeName(name string) string {
	// Last path element only (filepath.Base would treat "a:" as a drive on Windows).
	if i := strings.LastIndexAny(name, `/\`); i >= 0 {
		name = name[i+1:]
	}
	name = strings.Map(func(r rune) rune {
		if r < 32 || strings.ContainsRune(`<>:"/\|?*`, r) || (r >= 0x202A && r <= 0x202E) || (r >= 0x2066 && r <= 0x2069) {
			return '_'
		}
		return r
	}, name)
	name = strings.Trim(name, ". ")
	if len(name) > 120 {
		name = name[len(name)-120:]
	}
	if name == "" {
		return "file"
	}
	return name
}

// cleanCache deletes downloaded media older than a day.
func cleanCache(dir string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	cutoff := time.Now().Add(-24 * time.Hour)
	for _, e := range entries {
		if info, err := e.Info(); err == nil && !e.IsDir() && info.ModTime().Before(cutoff) {
			_ = os.Remove(filepath.Join(dir, e.Name()))
		}
	}
}

// thumbnail returns picture bytes for a message. Normally the small
// thumbnail embedded in it; with full (or for stickers, which rarely have
// a thumbnail) the original image.
func (a *App) thumbnail(ctx context.Context, chat, id string, full bool) []byte {
	m, err := a.db.getMsg(ctx, chat, id)
	if err != nil || len(m.raw) == 0 {
		return []byte{}
	}
	var msg waE2E.Message
	if proto.Unmarshal(m.raw, &msg) != nil {
		return []byte{}
	}
	original := func() []byte {
		if msg.ImageMessage == nil && msg.StickerMessage == nil || fileLength(&msg) > 16<<20 {
			return nil
		}
		data, err := a.cli.DownloadAny(ctx, &msg)
		if err != nil {
			return nil
		}
		return data
	}
	if full {
		if data := original(); len(data) > 0 {
			return data
		}
	}
	for _, t := range [][]byte{
		msg.GetImageMessage().GetJPEGThumbnail(),
		msg.GetVideoMessage().GetJPEGThumbnail(),
		msg.GetStickerMessage().GetPngThumbnail(),
	} {
		if len(t) > 0 {
			return t
		}
	}
	if !full {
		if data := original(); len(data) > 0 {
			return data
		}
	}
	return []byte{}
}

var usualExt = map[string]string{
	"image/jpeg":      ".jpg",
	"image/png":       ".png",
	"image/webp":      ".webp",
	"image/gif":       ".gif",
	"image/heic":      ".heic",
	"video/mp4":       ".mp4",
	"video/3gpp":      ".3gp",
	"video/quicktime": ".mov",
	"audio/ogg":       ".ogg",
	"audio/mpeg":      ".mp3",
	"audio/mp4":       ".m4a",
	"audio/aac":       ".aac",
	"application/pdf": ".pdf",
}

func extension(m *waE2E.Message) string {
	var mimeType string
	fallback := ".bin"
	switch {
	case m.ImageMessage != nil:
		mimeType, fallback = m.GetImageMessage().GetMimetype(), ".jpg"
	case m.VideoMessage != nil:
		mimeType, fallback = m.GetVideoMessage().GetMimetype(), ".mp4"
	case m.AudioMessage != nil:
		mimeType, fallback = m.GetAudioMessage().GetMimetype(), ".ogg"
	case m.StickerMessage != nil:
		mimeType, fallback = m.GetStickerMessage().GetMimetype(), ".webp"
	case m.DocumentMessage != nil:
		mimeType = m.GetDocumentMessage().GetMimetype()
	}
	mimeType, _, _ = strings.Cut(mimeType, ";")
	// The usual extension: the system's list is sorted, so JPEG would be ".jfif".
	if ext, ok := usualExt[strings.ToLower(strings.TrimSpace(mimeType))]; ok {
		return ext
	}
	if exts, err := mime.ExtensionsByType(mimeType); err == nil && len(exts) > 0 {
		return exts[0]
	}
	return fallback
}

// ---- logging --------------------------------------------------------------

type fileLogger struct {
	f      *os.File
	module string
}

func (l fileLogger) log(level, msg string, args ...any) {
	fmt.Fprintf(l.f, "%s [%s %s] %s\n", time.Now().Format(time.RFC3339), l.module, level, fmt.Sprintf(msg, args...))
}

func (l fileLogger) Warnf(msg string, args ...any)  { l.log("WARN", msg, args...) }
func (l fileLogger) Errorf(msg string, args ...any) { l.log("ERROR", msg, args...) }
func (l fileLogger) Infof(msg string, args ...any)  { l.log("INFO", msg, args...) }
func (l fileLogger) Debugf(msg string, args ...any) {}
func (l fileLogger) Sub(module string) waLog.Logger {
	return fileLogger{f: l.f, module: l.module + "/" + module}
}
