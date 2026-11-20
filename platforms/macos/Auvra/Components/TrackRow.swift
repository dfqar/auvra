import SwiftUI

struct TrackRow: View {
    let item: MediaItem
    let index: Int
    let active: Bool
    var loading = false
    var artworkFallback: String? = nil
    var readOnly = false
    let action: () -> Void
    var accessibilityAction = "Play"
    var status: String? = nil
    @State private var hovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        if readOnly {
            row
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("\(accessibilityAction) \(item.title) by \(item.artist)")
                .accessibilityValue(status ?? "")
        } else {
            Button(action: action) { row }
                .buttonStyle(.plain)
                .onHover { hovered = $0 }
                .animation(AppMotion.feedback(reduceMotion), value: hovered)
                .animation(AppMotion.feedback(reduceMotion), value: active)
                .accessibilityLabel("\(accessibilityAction) \(item.title) by \(item.artist)")
                .accessibilityValue(status ?? "")
                .help(item.capabilities.play ? "\(item.title) — \(item.artist)" : "Unavailable for playback")
        }
    }

    private var row: some View {
        HStack(spacing: 16) {
            ZStack {
                if loading { ProgressView().controlSize(.mini) }
                else if active || (hovered && !readOnly) {
                    Image(systemName: active ? "waveform" : "play.fill")
                } else {
                    Text(String(index + 1)).font(.callout.monospacedDigit())
                }
            }.foregroundStyle(active ? AuvraColors.accent : Color.secondary).frame(width: 26)
            ArtworkView(reference: item.artwork ?? artworkFallback).frame(width: 36, height: 36).clipShape(RoundedRectangle(cornerRadius: 4))
            VStack(alignment: .leading, spacing: 5) {
                Text(item.title).font(.system(size: 14, weight: .medium)).foregroundStyle(.primary).lineLimit(1)
                Text(item.artist).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 16)
            if let status {
                Text(status).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
        }
        .padding(.horizontal, 18).padding(.vertical, 14)
        .background(active ? AuvraColors.selection : hovered && !readOnly ? AuvraColors.hover : Color.clear,
                    in: RoundedRectangle(cornerRadius: 12))
        .contentShape(Rectangle())
    }
}

func formatTime(_ seconds: Double) -> String {
    guard seconds.isFinite, seconds > 0 else { return "0:00" }
    let value = Int(min(seconds, Double(Int.max / 2)))
    return "\(value / 60):" + String(format: "%02d", value % 60)
}
