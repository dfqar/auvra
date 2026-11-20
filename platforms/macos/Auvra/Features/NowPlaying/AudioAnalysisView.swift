import SwiftUI

/// Debug presentation of compact observations. Audio and FFT remain Rust-owned.
struct AudioAnalysisView: View {
    let player: PlayerStore
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Audio Analysis").font(.headline)
            if let frame = player.audioAnalysis {
                spectrum(frame)
                HStack {
                    Text("20 Hz"); Spacer(); Text("\(Int(min(20_000, frame.sampleRate / 2))) Hz")
                }.font(.caption2).foregroundStyle(.secondary)
                let f = frame.features
                Text(String(format: "RMS %.4f · Transient %.2f · Centroid %.0f Hz", f.rms, f.transient, f.spectralCentroidHz))
                    .font(.caption.monospacedDigit())
                HStack(spacing: 12) {
                    band("Sub", f.subBass); band("Bass", f.bass); band("Low mid", f.lowMids)
                    band("Mid", f.mids); band("Presence", f.presence); band("High", f.highs)
                }
                Text(String(format: "Audio time %.3f s · %d Hz · FFT %d · Analysis %.0f/s · Drops %llu",
                            Double(f.playbackPositionMs) / 1000, frame.sampleRate, frame.windowFrames,
                            Double(frame.sampleRate) / Double(frame.hopFrames), frame.droppedFrames))
                    .font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
                Text("Measured after DSP and output volume. Spectrum uses a −72 to 0 dBFS display scale.")
                    .font(.caption2).foregroundStyle(.secondary)
            } else {
                Text(status).foregroundStyle(.secondary)
            }
        }
        .padding(16).frame(maxWidth: 620, alignment: .leading)
        .background(.black.opacity(0.45), in: RoundedRectangle(cornerRadius: 12))
    }
    private var status: String {
        guard player.snapshot?.capabilities.pcmAnalysis == true else { return "Audio analysis is unavailable for this source." }
        if player.snapshot?.state == .paused { return "Paused · analysis cleared. Resume to measure audio." }
        if player.snapshot?.state == .playing { return "Waiting for a complete audio window…" }
        return "Play a local audio file to measure its spectrum."
    }
    private func band(_ title: String, _ value: Float) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).foregroundStyle(.secondary)
            Text(String(format: "%.3f", value)).monospacedDigit()
        }.font(.caption2)
    }
    private func spectrum(_ frame: AudioAnalysisFrame) -> some View {
        Canvas { context, size in
            let step = size.width / CGFloat(frame.spectrum.count)
            for (index, amplitude) in frame.spectrum.enumerated() {
                let level = max(0, min(1, (20 * log10(max(Double(amplitude), 0.000_000_1)) + 72) / 72))
                let height = CGFloat(level) * size.height
                let rect = CGRect(x: CGFloat(index) * step, y: size.height - height, width: max(1, step - 2), height: max(1, height))
                context.fill(Path(roundedRect: rect, cornerRadius: 2), with: .color(.mint.opacity(0.85)))
            }
        }.frame(height: 88)
        .accessibilityLabel("Audio spectrum, 32 frequency bands")
        .accessibilityValue("20 Hz to \(min(20_000, frame.sampleRate / 2)) Hz")
    }
}
