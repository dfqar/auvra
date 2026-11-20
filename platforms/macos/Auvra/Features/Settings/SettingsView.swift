import SwiftUI

struct SettingsView: View {
    let player: PlayerStore
    let still: StillStore
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage(AppAppearance.preferenceKey) private var appearance = AppAppearance.system
    @AppStorage("AuvraArtworkTransitions") private var artworkTransitions = true
    @AppStorage("AuvraAnimateSpaces") private var animateSpaces = true
    @AppStorage("AuvraStillFrameRate") private var frameRate = 60
    @AppStorage("AuvraMiniAlwaysOnTop") private var miniOnTop = true
    @AppStorage(LyricsService.musixmatchPreferenceKey) private var experimentalMusixmatch = false
    @State private var presetName = ""
    @AppStorage("AuvraSettingsTab") private var selectedTab = "general"
    private let labels = ["80 Hz", "300 Hz", "1 kHz", "4 kHz", "12 kHz"]

    var body: some View {
        TabView(selection: $selectedTab) {
            Form {
                Section("Appearance") {
                    Picker("Theme", selection: $appearance) {
                        ForEach(AppAppearance.allCases) { Text($0.title).tag($0) }
                    }.accessibilityLabel("Appearance theme")
                    Toggle("Artwork transitions", isOn: $artworkTransitions)
                    Toggle("Animate background", isOn: $animateSpaces)
                    Toggle("Music-responsive light", isOn: Binding(get: { player.reactiveLighting }, set: player.setReactiveLighting))
                    Text(player.lightingStatus).font(.caption).foregroundStyle(.secondary).accessibilityLabel("Music light status").accessibilityValue(player.lightingStatus)
                    Text("System Reduce Motion also disables transitions and moving spaces.")
                        .font(.caption).foregroundStyle(.secondary)
                    Picker("Frame rate", selection: $frameRate) {
                        Text("60 fps").tag(60); Text("Match display (up to 120 fps)").tag(0)
                    }
                    Text("Hidden spaces stop rendering. Automatic power and thermal limits still apply.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section("Windows") { Toggle("Keep Mini Player on top", isOn: $miniOnTop) }
                Section("Playback") {
                    Text("Your queue, volume and sound settings are saved automatically. Restored tracks wait for Play.")
                        .foregroundStyle(.secondary)
                    Button(player.isRestarting ? "Restarting…" : "Restart Player") { Task { await player.restartPlayer() } }.disabled(player.isRestarting)
                    Text("Restores the saved queue on pause and reconnects the player.").font(.caption).foregroundStyle(.secondary)
                }
                Section("Spatial Audio") {
                    Text("While a local track plays on compatible Bluetooth headphones, choose Spatialize Stereo in macOS Control Center → Sound.")
                        .font(.caption).foregroundStyle(.secondary)
                    if #available(macOS 14.2, *) {
                        Toggle("Spotify spatial stereo (experimental)", isOn: Binding(get: { player.spotifySpatialStereo }, set: player.setSpotifySpatialStereo))
                        Text("Routes audio from the Auvra playback device on this Mac. macOS may ask for system audio permission.")
                            .font(.caption).foregroundStyle(.secondary)
                        if player.spotifySpatialStereo {
                            Text(player.spotifySpatialStereoStatus)
                                .font(.caption).foregroundStyle(.secondary)
                                .accessibilityIdentifier("spotify-spatial-stereo-status")
                        }
                    }
                }
                Section("Lyrics") {
                    Toggle("Experimental Musixmatch", isOn: $experimentalMusixmatch)
                    Text("Looks for synced lyrics through Musixmatch’s unofficial client API when the primary lookup has no timing.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.formStyle(.grouped).tabItem { Label("General", systemImage: "gearshape") }.tag("general")
            Form {
                Section {
                    Toggle("Enable equalizer", isOn: eqBinding(\.enabled))
                    Text(player.snapshot?.capabilities.equalizer == true ? (player.soundSettings.equalizer.enabled ? "Applied to this track." : "Available for this track. EQ bypassed.") : "Applies to local audio. This source plays without local EQ.")
                        .font(.caption).foregroundStyle(.secondary)
                    gain("Preamp", value: eqBinding(\.preampDb), range: -24...6)
                    ForEach(0..<5, id: \.self) { index in
                        gain(labels[index], value: Binding(get: { Double(player.soundSettings.equalizer.bandsDb[index]) },
                            set: { value in player.updateSound { $0.equalizer.bandsDb[index] = Float(value) } }), range: -12...12)
                    }
                    Toggle("Protect peaks", isOn: eqBinding(\.peakProtection))
                    Text("Softens peaks above −0.92 dBFS when EQ is enabled. Lower Preamp to preserve more headroom.")
                        .font(.caption).foregroundStyle(.secondary)
                } header: { Text("Local equalizer") }
                Section("Presets") {
                    HStack {
                        Menu("Apply Preset") {
                            Button("Flat") { apply([0, 0, 0, 0, 0], preamp: 0) }
                            Button("Warm") { apply([3, 1, 0, -1, -2], preamp: -4) }
                            Button("Voice") { apply([-3, -1, 2, 2, -2], preamp: -4) }
                            Button("Bright") { apply([-1, 0, 0, 2, 3], preamp: -4) }
                            if !player.soundSettings.presets.isEmpty {
                                Divider()
                                ForEach(player.soundSettings.presets, id: \.name) { preset in
                                    Button(preset.name) { player.updateSound { $0.equalizer = preset.equalizer } }
                                }
                            }
                        }
                        Spacer()
                        Button("Reset EQ") { player.updateSound { $0.equalizer = EqualizerSettings() } }
                    }
                    HStack {
                        TextField("Preset name", text: $presetName).accessibilityLabel("New EQ preset name")
                        Button("Save") {
                            let name = presetName.trimmingCharacters(in: .whitespacesAndNewlines)
                            player.updateSound { value in
                                value.presets.removeAll { $0.name == name }
                                value.presets.append(EqualizerPreset(name: name, equalizer: value.equalizer))
                            }; presetName = ""
                        }.disabled(!canSavePreset)
                    }
                    ForEach(player.soundSettings.presets, id: \.name) { preset in
                        HStack {
                            Text(preset.name); Spacer()
                            Button { player.updateSound { $0.presets.removeAll { $0.name == preset.name } } } label: { Image(systemName: "minus.circle") }
                                .accessibilityLabel("Remove preset \(preset.name)")
                        }
                    }
                }
                if let issue = player.snapshot?.sound?.issue {
                    Section {
                        Text(issue.message).foregroundStyle(AuvraColors.warning).textSelection(.enabled)
                        Button("Retry Sound Storage") { player.send(.retrySoundStorage) }
                    }
                } else {
                    Text(player.isSavingSound ? "Saving…" : player.snapshot?.sound?.ready == true ? "Sound settings saved on this Mac." : "Preparing sound settings…")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.formStyle(.grouped).tabItem { Label("Equalizer", systemImage: "slider.horizontal.3") }.tag("equalizer")
            Form { SpotifyAccountView(player: player) }.formStyle(.grouped).tabItem { Label("Account", systemImage: "person.crop.circle") }.tag("account")
            PrivacyView().tabItem { Label("Privacy", systemImage: "hand.raised") }.tag("privacy")
        }.frame(width: 540, height: 630).auvraAppearance()
            .animation(AppMotion.navigation(reduceMotion), value: selectedTab)
            .disabled(player.isRestarting)
            .onChange(of: experimentalMusixmatch) { _, _ in player.lyrics.refreshOnlineLyrics() }
    }
    private var canSavePreset: Bool {
        let name = presetName.trimmingCharacters(in: .whitespacesAndNewlines)
        return !name.isEmpty && name.utf8.count <= 80 && !name.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
            && (player.soundSettings.presets.count < 16 || player.soundSettings.presets.contains { $0.name == name })
    }
    private func eqBinding<T>(_ key: WritableKeyPath<EqualizerSettings, T>) -> Binding<T> {
        Binding(get: { player.soundSettings.equalizer[keyPath: key] }, set: { value in player.updateSound { $0.equalizer[keyPath: key] = value } })
    }
    private func gain(_ label: String, value: Binding<Float>, range: ClosedRange<Double>) -> some View {
        gain(label, value: Binding(get: { Double(value.wrappedValue) }, set: { value.wrappedValue = Float($0) }), range: range)
    }
    private func gain(_ label: String, value: Binding<Double>, range: ClosedRange<Double>) -> some View {
        HStack {
            Text(label).frame(width: 64, alignment: .leading)
            Slider(value: value, in: range, step: 0.5).accessibilityLabel(label)
                .accessibilityValue(String(format: "%+.1f decibels", value.wrappedValue))
            Text(String(format: "%+.1f dB", value.wrappedValue)).monospacedDigit().frame(width: 66, alignment: .trailing)
        }
    }
    private func apply(_ bands: [Float], preamp: Float) {
        player.updateSound { $0.equalizer = EqualizerSettings(enabled: true, preampDb: preamp, bandsDb: bands, peakProtection: true) }
    }
}

/// Read-only information: opening this view does not fetch account data or lyrics.
struct PrivacyView: View {
    var body: some View {
        Form {
            Section("Saved on this Mac") {
                paragraph("Library metadata, file-access bookmarks, playback positions, favorites, settings and imported lyrics are stored on this Mac. Auvra does not upload your local audio files.")
            }
            Section("Account & playback") {
                paragraph("Auvra’s saved Spotify login and playback key use macOS Keychain. The official playback engine also keeps its own local session and audio cache.")
                paragraph("Searches, playback commands and library changes go to Spotify. Artwork and profile images load from Spotify image servers. Pairing discovers the Auvra playback device on your local network.")
                paragraph("If you grant listening progress access, Auvra reads saved positions and completion for Spotify episodes and chapters. Catalog progress stays in memory.")
            }
            Section("Online lyrics") {
                paragraph("When online lyrics are needed, LRCLIB receives the track title, first artist, album and duration. For Spotify tracks, GitHub also receives the track ID for an AMLL TTML DB lookup.")
                paragraph("If LRCLIB has no synced lyrics or is unavailable, Unison can receive the track title and first artist for a supplemental search.")
                paragraph("When a public project contact is configured, lrc.red can receive the track title, first artist and app contact.")
                paragraph("When enabled in General, experimental Musixmatch receives track metadata and a Spotify track ID if available. Its anonymous token is kept in memory.")
                paragraph("These requests carry app identification and a network IP address, without Spotify credentials. Imported lyrics take priority; online lyrics are cached only in memory.")
                HStack(spacing: 16) {
                    Link("LRCLIB", destination: URL(string: "https://lrclib.net/docs")!)
                    Link("AMLL TTML DB", destination: URL(string: "https://github.com/amll-dev/amll-ttml-db")!)
                }
            }
            Section("Music-responsive light") {
                paragraph("Frost analyses local audio or Auvra’s own playback output on this Mac. It does not use the microphone or other apps’ audio, and does not save audio recordings. You can turn it off in General.")
            }
            Section("Disconnecting") {
                paragraph("Disconnect in Account removes the saved login, playback key and engine session when it succeeds. Your Spotify library, local music files, local library data, favorites and imported lyrics remain.")
            }
            Section("Diagnostics & services") {
                paragraph("Auvra includes no analytics or automatic diagnostics uploader of its own. Spotify, the official playback engine, lyric services and macOS handle their own service and diagnostic data.")
            }
        }.formStyle(.grouped)
    }
    private func paragraph(_ text: String) -> some View {
        Text(text).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
    }
}
