package com.misebanai.agent

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Base64
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.*
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.*
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.text.SimpleDateFormat
import java.util.*
import java.util.concurrent.Executors

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    AgentApp()
                }
            }
        }
    }
}

@Composable
fun AgentApp() {
    val context = LocalContext.current
    val prefs = context.getSharedPreferences("miseban", 0)

    var token by remember { mutableStateOf(prefs.getString("apiToken", "") ?: "") }
    var apiBase by remember { mutableStateOf(prefs.getString("apiBase", "https://api.misebanai.com") ?: "") }
    var isRunning by remember { mutableStateOf(false) }
    var peopleCount by remember { mutableIntStateOf(0) }
    var hasCameraPermission by remember {
        mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }

    val permLauncher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        hasCameraPermission = granted
    }

    val scope = rememberCoroutineScope()
    var captureJob: Job? by remember { mutableStateOf(null) }

    // Image capture executor
    val cameraExecutor = remember { Executors.newSingleThreadExecutor() }
    val imageCapture = remember { ImageCapture.Builder().build() }

    fun sendFrame(jpeg: ByteArray) {
        scope.launch(Dispatchers.IO) {
            try {
                val deviceId = android.provider.Settings.Secure.getString(
                    context.contentResolver, android.provider.Settings.Secure.ANDROID_ID)
                val body = JSONObject().apply {
                    put("camera_id", "android-$deviceId")
                    put("timestamp", SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss'Z'", Locale.US).apply {
                        timeZone = TimeZone.getTimeZone("UTC")
                    }.format(Date()))
                    put("jpeg_bytes", Base64.encodeToString(jpeg, Base64.NO_WRAP))
                    put("resolution", JSONObject().apply {
                        put("width", 1280); put("height", 720)
                    })
                }

                val conn = URL("$apiBase/api/v1/frames").openConnection() as HttpURLConnection
                conn.apply {
                    requestMethod = "POST"
                    setRequestProperty("Content-Type", "application/json")
                    setRequestProperty("Authorization", "Bearer $token")
                    doOutput = true
                    outputStream.write(body.toString().toByteArray())
                }

                val response = conn.inputStream.bufferedReader().readText()
                val json = JSONObject(response)
                withContext(Dispatchers.Main) {
                    peopleCount = json.optInt("people_count", 0)
                }
            } catch (e: Exception) {
                Log.e("MisebanAI", "Send failed: ${e.message}")
            }
        }
    }

    Column(modifier = Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        // Title
        Text("MisebanAI", fontSize = 28.sp, style = MaterialTheme.typography.headlineLarge)

        // Status
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Surface(color = if (isRunning) Color.Green else Color.Gray, shape = MaterialTheme.shapes.small) {
                Spacer(modifier = Modifier.size(10.dp))
            }
            Text(if (isRunning) "送信中" else "停止中", style = MaterialTheme.typography.bodyMedium)
            Spacer(modifier = Modifier.weight(1f))
            Button(
                onClick = {
                    if (isRunning) {
                        captureJob?.cancel()
                        isRunning = false
                    } else {
                        if (!hasCameraPermission) {
                            permLauncher.launch(Manifest.permission.CAMERA)
                            return@Button
                        }
                        prefs.edit().putString("apiToken", token).putString("apiBase", apiBase).apply()
                        isRunning = true
                        captureJob = scope.launch {
                            while (isActive) {
                                imageCapture.takePicture(cameraExecutor, object : ImageCapture.OnImageCapturedCallback() {
                                    override fun onCaptureSuccess(image: ImageProxy) {
                                        val buffer = image.planes[0].buffer
                                        val bytes = ByteArray(buffer.remaining())
                                        buffer.get(bytes)
                                        image.close()
                                        sendFrame(bytes)
                                    }
                                })
                                delay(5000)
                            }
                        }
                    }
                },
                colors = ButtonDefaults.buttonColors(containerColor = if (isRunning) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary)
            ) {
                Text(if (isRunning) "停止" else "開始")
            }
        }

        // Camera preview
        if (hasCameraPermission) {
            val lifecycleOwner = LocalLifecycleOwner.current
            AndroidView(
                factory = { ctx ->
                    PreviewView(ctx).also { previewView ->
                        val cameraProviderFuture = ProcessCameraProvider.getInstance(ctx)
                        cameraProviderFuture.addListener({
                            val cameraProvider = cameraProviderFuture.get()
                            val preview = Preview.Builder().build().also { it.surfaceProvider = previewView.surfaceProvider }
                            cameraProvider.unbindAll()
                            cameraProvider.bindToLifecycle(lifecycleOwner, CameraSelector.DEFAULT_BACK_CAMERA, preview, imageCapture)
                        }, ContextCompat.getMainExecutor(ctx))
                    }
                },
                modifier = Modifier.fillMaxWidth().height(220.dp)
            )
        } else {
            OutlinedCard(modifier = Modifier.fillMaxWidth().height(220.dp)) {
                Box(contentAlignment = Alignment.Center, modifier = Modifier.fillMaxSize()) {
                    Button(onClick = { permLauncher.launch(Manifest.permission.CAMERA) }) {
                        Text("カメラアクセスを許可")
                    }
                }
            }
        }

        // People count
        if (isRunning) {
            Card(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier.padding(16.dp).fillMaxWidth(),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    Text("$peopleCount", fontSize = 72.sp, style = MaterialTheme.typography.displayLarge)
                    Text("現在の来客数", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        } else {
            // Setup fields
            OutlinedTextField(
                value = token,
                onValueChange = { token = it },
                label = { Text("APIトークン") },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true
            )
            OutlinedTextField(
                value = apiBase,
                onValueChange = { apiBase = it },
                label = { Text("APIエンドポイント") },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true
            )
        }
    }
}
