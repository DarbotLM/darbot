import AsyncStorage from "@react-native-async-storage/async-storage";
import React, { useEffect, useRef, useState } from "react";
import {
  ActivityIndicator, Alert, BackHandler, Image, KeyboardAvoidingView, Linking, Modal, Platform,
  Pressable, ScrollView, StatusBar, StyleSheet, Text, TextInput, View,
} from "react-native";
import { SafeAreaProvider, SafeAreaView } from "react-native-safe-area-context";
import { WebView } from "react-native-webview";
import { isExternalHttps, isSameOrigin, normalizeEndpoint } from "./endpoint";

const STORE = "darbot.mobile.endpoint.v1";
const palette = { black:"#000000", surface:"#101014", raised:"#1c1c22", text:"#f5f5f7",
  muted:"#a6a6b1", line:"#303038", violet:"#9980ff", cyan:"#08a9ef", navy:"#080c2e" };

function Action({ title, onPress, primary = false, disabled = false }: {
  title:string; onPress:()=>void; primary?:boolean; disabled?:boolean;
}) {
  return <Pressable accessibilityRole="button" disabled={disabled}
    onPress={onPress} style={[styles.action, primary && styles.primary, disabled && {opacity:0.5}]}>
    <Text style={[styles.actionText, primary && styles.primaryText]}>{title}</Text>
  </Pressable>;
}
function Shell() {
  const [endpoint, setEndpoint] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [ready, setReady] = useState(false);
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [networkError, setNetworkError] = useState(false);
  const [blockedNavigation, setBlockedNavigation] = useState(false);
  const [loading, setLoading] = useState(true);
  const [canBack, setCanBack] = useState(false);
  const [revision, setRevision] = useState(0);
  const web = useRef<WebView>(null);

  useEffect(() => {
    let alive = true;
    void AsyncStorage.getItem(STORE).then((stored) => {
      if (!alive || !stored) return;
      const origin = normalizeEndpoint(stored);
      setEndpoint(origin); setDraft(origin);
    }).catch(() => { if (alive) setError("Could not restore the saved server."); })
      .finally(() => { if (alive) setReady(true); });
    return () => { alive = false; };
  }, []);

  useEffect(() => {
    const listener = BackHandler.addEventListener("hardwareBackPress", () => {
      if (editing && endpoint) { setEditing(false); return true; }
      if (canBack && !networkError && !editing) { web.current?.goBack(); return true; }
      return false;
    });
    return () => listener.remove();
  }, [canBack, editing, endpoint, networkError]);

  async function connect() {
    if (saving) return;
    setSaving(true); setError("");
    try {
      const origin = normalizeEndpoint(draft);
      await AsyncStorage.setItem(STORE, origin);
      setEndpoint(origin); setDraft(origin); setEditing(false);
      setCanBack(false); setNetworkError(false); setBlockedNavigation(false); setLoading(true); setRevision(n => n + 1);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : "Could not save the server.");
    } finally { setSaving(false); }
  }
  function external(url: string) {
    if (!isExternalHttps(url)) return;
    Alert.alert("Open outside Darbot?", new URL(url).hostname,
      [{text:"Cancel",style:"cancel"},{text:"Open browser",onPress:()=>{
        void Linking.openURL(url).catch(() => Alert.alert("Could not open this link."));
      }}]);
  }
  function settings() { setDraft(endpoint ?? ""); setError(""); setEditing(true); }
  function retry() { setNetworkError(false); setBlockedNavigation(false); setLoading(true); setCanBack(false); setRevision(n => n + 1); }

  if (!ready) return <View style={styles.center}><ActivityIndicator color={palette.violet}/></View>;
  const serverForm = (
    <KeyboardAvoidingView style={styles.flex} behavior={Platform.OS === "ios" ? "padding" : undefined}>
      <ScrollView keyboardShouldPersistTaps="handled" contentContainerStyle={styles.setup}>
        <Image accessibilityIgnoresInvertColors source={require("../assets/darbot-orb.png")} style={styles.mark}/>
        <Text style={styles.title}>Your agents. Anywhere.</Text>
        <Text style={styles.description}>Connect to your Darbot deployment. Conversations, agents and tools remain on your server.</Text>
        <Text style={styles.label}>Darbot server</Text>
        <TextInput accessibilityLabel="Darbot HTTPS server" value={draft} onChangeText={setDraft}
          placeholder="https://your-darbot-server.example" placeholderTextColor={palette.muted}
          autoCapitalize="none" autoCorrect={false} keyboardType="url" returnKeyType="go"
          onSubmitEditing={() => void connect()} style={styles.input}/>
        {!!error && <Text accessibilityRole="alert" style={styles.error}>{error}</Text>}
        <Action title={saving ? "Connecting…" : "Connect"} primary disabled={saving} onPress={() => void connect()}/>
        {endpoint && <Action title="Cancel" onPress={() => setEditing(false)}/>}
        <Text style={styles.note}>Use a trusted HTTPS deployment reachable from this device. Sign in with your deployment's existing account.</Text>
      </ScrollView>
    </KeyboardAvoidingView>
  );
  if (!endpoint) return serverForm;
  return <View style={styles.flex}>
    <Modal visible={editing} animationType="slide" presentationStyle="fullScreen" onRequestClose={() => setEditing(false)}>
      <SafeAreaView style={styles.flex}>{serverForm}</SafeAreaView>
    </Modal>
    <View style={styles.connection}>
      <View style={styles.row}>
        {canBack && <Pressable accessibilityRole="button" accessibilityLabel="Go back" style={styles.back}
          onPress={() => web.current?.goBack()}><Text style={styles.actionText}>‹</Text></Pressable>}
        <Text numberOfLines={1} style={styles.host}>{new URL(endpoint).hostname}</Text>
      </View>
      <Pressable accessibilityRole="button" accessibilityLabel="Connection settings" onPress={settings} style={styles.connectionButton}>
        <Text style={styles.connectionText}>Server</Text>
      </Pressable>
    </View>
    {networkError || blockedNavigation ? <View style={styles.failure}>
      <Text style={styles.title}>{blockedNavigation ? "Navigation blocked" : "Cannot reach Darbot"}</Text>
      <Text style={styles.description}>{blockedNavigation ? "The page tried to leave your configured Darbot server." : "Check your network, VPN, certificate and server address."}</Text>
      <Action title="Retry" primary onPress={retry}/><Action title="Change server" onPress={settings}/>
    </View> : <View style={styles.flex}>
      <WebView key={endpoint + revision} ref={web} source={{uri:endpoint}}
        style={styles.flex} containerStyle={styles.flex} originWhitelist={["*"]}
        javaScriptEnabled domStorageEnabled sharedCookiesEnabled={false}
        thirdPartyCookiesEnabled={false} mixedContentMode="never"
        allowFileAccess={false} allowFileAccessFromFileURLs={false}
        allowUniversalAccessFromFileURLs={false}
        javaScriptCanOpenWindowsAutomatically={false}
        allowsBackForwardNavigationGestures
        onNavigationStateChange={state => {
          // A secondary guard for platform-specific redirect paths not reported by the request hook.
          if (state.url !== "about:blank" && !isSameOrigin(state.url, endpoint)) {
            web.current?.stopLoading(); setBlockedNavigation(true); setLoading(false); return;
          }
          setCanBack(state.canGoBack);
        }}
        onShouldStartLoadWithRequest={request => {
          // Existing sandboxed components can load in child frames. Top-level navigation is origin-bound.
          if (request.isTopFrame === false) return true;
          if (isSameOrigin(request.url, endpoint)) return true;
          if (request.url !== "about:blank") external(request.url);
          return false;
        }}
        onOpenWindow={event => external(event.nativeEvent.targetUrl)}
        onLoadStart={() => setLoading(true)}
        onLoadEnd={() => setLoading(false)}
        onError={() => {setNetworkError(true); setLoading(false);}}
        onContentProcessDidTerminate={retry}
        onRenderProcessGone={retry}
      />
      {loading && <View pointerEvents="none" style={styles.progress}><ActivityIndicator size="small" color={palette.violet}/></View>}
    </View>}
  </View>;
}
export default function App() {
  return <SafeAreaProvider><SafeAreaView style={styles.flex} edges={["top","bottom","left","right"]}>
    <StatusBar barStyle="light-content" backgroundColor={palette.black}/><Shell/>
  </SafeAreaView></SafeAreaProvider>;
}
const styles = StyleSheet.create({
  flex:{flex:1,backgroundColor:palette.black},
  center:{flex:1,alignItems:"center",justifyContent:"center",backgroundColor:palette.black},
  setup:{flexGrow:1,justifyContent:"center",padding:24,gap:16,maxWidth:540,width:"100%",alignSelf:"center"},
  mark:{width:64,height:64,resizeMode:"contain"},
  title:{fontSize:32,letterSpacing:-1.3,fontWeight:"600",color:palette.text},
  description:{fontSize:16,lineHeight:24,color:palette.muted},
  label:{color:palette.text,fontSize:14,fontWeight:"600",marginTop:8},
  input:{minHeight:54,borderWidth:1,borderColor:palette.line,borderRadius:18,backgroundColor:palette.raised,color:palette.text,fontSize:16,paddingHorizontal:16},
  action:{minHeight:48,borderRadius:24,borderWidth:1,borderColor:palette.line,alignItems:"center",justifyContent:"center",paddingHorizontal:16},
  actionText:{color:palette.text,fontSize:16,fontWeight:"600"},
  primary:{backgroundColor:palette.text,borderColor:palette.text},
  primaryText:{color:palette.navy},
  note:{fontSize:13,lineHeight:19,color:palette.muted},
  error:{color:"#ff9696",fontSize:14},
  connection:{minHeight:44,flexDirection:"row",justifyContent:"space-between",alignItems:"center",paddingHorizontal:12,borderBottomWidth:StyleSheet.hairlineWidth,borderColor:palette.line},
  row:{flex:1,flexDirection:"row",alignItems:"center",minWidth:0},
  host:{fontSize:12,color:palette.muted,flexShrink:1},
  back:{minWidth:44,minHeight:44,justifyContent:"center",alignItems:"center"},
  connectionButton:{minHeight:44,minWidth:60,alignItems:"center",justifyContent:"center"},
  connectionText:{color:palette.violet,fontWeight:"600",fontSize:13},
  failure:{flex:1,justifyContent:"center",padding:24,gap:16},
  progress:{position:"absolute",top:12,right:12,backgroundColor:palette.surface,borderRadius:20,padding:8},
});
