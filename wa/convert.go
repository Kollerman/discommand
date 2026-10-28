package main

import (
	"fmt"
	"strings"

	"go.mau.fi/whatsmeow/proto/waE2E"
)

// content describes what a message shows. kind is "" for messages that
// carry nothing displayable (key distribution, protocol noise, ...).
type content struct {
	kind  string
	text  string
	file  string
	media bool
	ctx   *waE2E.ContextInfo
}

func extract(m *waE2E.Message) content {
	if m == nil {
		return content{}
	}
	switch {
	case m.Conversation != nil:
		return content{kind: "text", text: m.GetConversation()}
	case m.ExtendedTextMessage != nil:
		e := m.GetExtendedTextMessage()
		return content{kind: "text", text: e.GetText(), ctx: e.GetContextInfo()}
	case m.ImageMessage != nil:
		e := m.GetImageMessage()
		return content{kind: "image", text: e.GetCaption(), media: true, ctx: e.GetContextInfo()}
	case m.VideoMessage != nil:
		e := m.GetVideoMessage()
		kind := "video"
		if e.GetGifPlayback() {
			kind = "gif"
		}
		return content{kind: kind, text: e.GetCaption(), media: true, ctx: e.GetContextInfo()}
	case m.AudioMessage != nil:
		e := m.GetAudioMessage()
		kind := "audio"
		if e.GetPTT() {
			kind = "voice"
		}
		return content{kind: kind, text: duration(e.GetSeconds()), media: true, ctx: e.GetContextInfo()}
	case m.DocumentMessage != nil:
		e := m.GetDocumentMessage()
		return content{kind: "document", text: e.GetCaption(), file: e.GetFileName(), media: true, ctx: e.GetContextInfo()}
	case m.StickerMessage != nil:
		e := m.GetStickerMessage()
		return content{kind: "sticker", media: true, ctx: e.GetContextInfo()}
	case m.LocationMessage != nil:
		e := m.GetLocationMessage()
		text := fmt.Sprintf("%s https://maps.google.com/?q=%f,%f", e.GetName(), e.GetDegreesLatitude(), e.GetDegreesLongitude())
		return content{kind: "location", text: strings.TrimSpace(text), ctx: e.GetContextInfo()}
	case m.LiveLocationMessage != nil:
		e := m.GetLiveLocationMessage()
		return content{kind: "location", text: fmt.Sprintf("live location https://maps.google.com/?q=%f,%f", e.GetDegreesLatitude(), e.GetDegreesLongitude()), ctx: e.GetContextInfo()}
	case m.ContactMessage != nil:
		e := m.GetContactMessage()
		return content{kind: "contact", text: e.GetDisplayName(), ctx: e.GetContextInfo()}
	case m.ContactsArrayMessage != nil:
		e := m.GetContactsArrayMessage()
		names := make([]string, 0, len(e.GetContacts()))
		for _, c := range e.GetContacts() {
			names = append(names, c.GetDisplayName())
		}
		return content{kind: "contact", text: strings.Join(names, ", "), ctx: e.GetContextInfo()}
	}
	for _, p := range []*waE2E.PollCreationMessage{m.GetPollCreationMessage(), m.GetPollCreationMessageV2(), m.GetPollCreationMessageV3()} {
		if p == nil {
			continue
		}
		opts := make([]string, 0, len(p.GetOptions()))
		for _, o := range p.GetOptions() {
			opts = append(opts, "○ "+o.GetOptionName())
		}
		return content{kind: "poll", text: p.GetName() + "\n" + strings.Join(opts, "\n"), ctx: p.GetContextInfo()}
	}
	return content{}
}

func duration(secs uint32) string {
	if secs == 0 {
		return ""
	}
	return fmt.Sprintf("%d:%02d", secs/60, secs%60)
}

// preview is a one-line summary used for quoted replies.
func preview(m *waE2E.Message) string {
	c := extract(m)
	text := strings.ReplaceAll(c.text, "\n", " ")
	switch c.kind {
	case "", "text":
		return text
	case "document":
		return strings.TrimSpace("[document] " + c.file + " " + text)
	default:
		return strings.TrimSpace("[" + c.kind + "] " + text)
	}
}
