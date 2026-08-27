// notify — notifications are an app, not a feature.
//
// Tails ply's events journal (a file, like everything else) and posts
// matching events to a webhook. Discord and Slack both accept the payload
// (each ignores the other's text field); anything else gets the full
// event JSON alongside.
//
//	WEBHOOK_URL    required — where to POST
//	NOTIFY_EVENTS  csv filter (default "deploy,deploy-failed,instance-restart")
//	PLY_EVENTS     journal path (default /ply/host/apps/events.log — the
//	               [requests] link, granted with --grant-links)
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"time"
)

type event struct {
	TS     int64  `json:"ts"`
	App    string `json:"app"`
	Event  string `json:"event"`
	Detail string `json:"detail"`
}

func main() {
	webhook := os.Getenv("WEBHOOK_URL")
	if webhook == "" {
		log.Fatal("WEBHOOK_URL is required")
	}
	path := env("PLY_EVENTS", "/ply/host/apps/events.log")
	wanted := map[string]bool{}
	for _, k := range strings.Split(env("NOTIFY_EVENTS", "deploy,deploy-failed,instance-restart"), ",") {
		wanted[strings.TrimSpace(k)] = true
	}
	host, _ := os.Hostname()
	log.Printf("notify: watching %s -> %s (events: %s)", path, redact(webhook), env("NOTIFY_EVENTS", "deploy,deploy-failed,instance-restart"))

	// start at the end: history is history, notifications are news
	offset := int64(0)
	if info, err := os.Stat(path); err == nil {
		offset = info.Size()
	}
	for {
		time.Sleep(2 * time.Second)
		info, err := os.Stat(path)
		if err != nil {
			continue // ring not born yet, or rotating right now
		}
		if info.Size() < offset {
			offset = 0 // ring rotated under us: the file restarted
		}
		if info.Size() == offset {
			continue
		}
		f, err := os.Open(path)
		if err != nil {
			continue
		}
		if _, err := f.Seek(offset, io.SeekStart); err == nil {
			raw, _ := io.ReadAll(f)
			offset += int64(len(raw))
			for _, line := range strings.Split(string(raw), "\n") {
				if line == "" {
					continue
				}
				var e event
				if json.Unmarshal([]byte(line), &e) != nil || !wanted[e.Event] {
					continue
				}
				post(webhook, host, e)
			}
		}
		f.Close()
	}
}

func post(webhook, host string, e event) {
	mark := map[string]string{
		"deploy": "✅", "deploy-failed": "❌", "instance-restart": "💥",
		"scale": "↕️", "restart": "🔄", "terminal": "⌨️",
	}[e.Event]
	text := fmt.Sprintf("%s **%s** · %s — %s", mark, e.App, e.Event, e.Detail)
	if host != "" {
		text += fmt.Sprintf(" _(%s)_", host)
	}
	body, _ := json.Marshal(map[string]any{
		"content": text, // Discord
		"text":    text, // Slack
		"event":   e,    // everyone else gets the data too
	})
	for attempt := 0; attempt < 3; attempt++ {
		resp, err := http.Post(webhook, "application/json", bytes.NewReader(body))
		if err == nil {
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
			if resp.StatusCode < 300 {
				return
			}
			log.Printf("notify: webhook answered %d", resp.StatusCode)
		} else {
			log.Printf("notify: %v", err)
		}
		time.Sleep(time.Duration(attempt+1) * 2 * time.Second)
	}
}

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func redact(url string) string {
	if i := strings.LastIndex(url, "/"); i > 30 {
		return url[:30] + "…"
	}
	return url
}
