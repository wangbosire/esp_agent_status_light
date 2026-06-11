#include <BLEDevice.h>
#include <BLEServer.h>
#include <BLEUtils.h>
#include <BLE2902.h>

// =====================================================
// ESP32-C3 SuperMini + 原玩具公共正极灯板：BLE 蓝牙控制增强版
//
// 接线方式：
// ESP32 3.3V  -> 原灯板 + / 原电池正极
// ESP32 IO2   -> 220Ω -> L1 控制点 = 绿灯
// ESP32 IO3   -> 220Ω -> L2 控制点 = 黄灯
// ESP32 IO4   -> 220Ω -> L3 控制点 = 红灯
//
// 注意：
// 1. 原灯板 - / 原电池负极 第一版先不要接。
// 2. 公共正极：GPIO LOW = 灯亮，GPIO HIGH = 灯灭。
// 3. 默认开机模式：demo
// 4. 除 off、traffic 外，其他模式最多运行 15 分钟，然后自动进入 traffic。
// 5. traffic 最多运行 20 分钟，然后自动 off。
// =====================================================

const char* BLE_DEVICE_NAME = "AgentStatusLight";

#define SERVICE_UUID        "b8b7e001-7a6b-4f4f-9a8b-11c0ffee0001"
#define MODE_CHAR_UUID      "b8b7e002-7a6b-4f4f-9a8b-11c0ffee0001"

// 你的实测：L1=绿灯，L2=黄灯，L3=红灯
const int GREEN_PIN = 2;   // IO2 -> L1 绿灯
const int YELLOW_PIN = 3;  // IO3 -> L2 黄灯
const int RED_PIN = 4;     // IO4 -> L3 红灯

const int PWM_FREQ = 5000;
const int PWM_RESOLUTION = 8;

// 红灯偏弱，所以红灯单独增强
const int RED_MAX = 255;
const int YELLOW_MAX = 220;
const int GREEN_MAX = 220;

// 这里统一采用 TECHNICAL_DESIGN.md / README 约定的正式超时规则：
// 1. 普通模式最多运行 15 分钟，避免灯长时间保持高亮。
// 2. traffic 最多运行 20 分钟，随后自动熄灭。
const unsigned long NORMAL_MODE_TIMEOUT_MS = 15UL * 60UL * 1000UL;   // 15 分钟
const unsigned long TRAFFIC_MODE_TIMEOUT_MS = 20UL * 60UL * 1000UL; // 20 分钟

String currentMode = "demo";
unsigned long modeStart = 0;

BLEServer* pServer = nullptr;
BLECharacteristic* pModeCharacteristic = nullptr;
bool deviceConnected = false;


// =====================================================
// 基础工具函数：公共正极反相输出
// =====================================================

void writeLed(int pin, int value) {
  value = constrain(value, 0, 255);
  // 公共正极灯板需要反相：逻辑亮度越高，PWM 实际输出值越低。
  int pwmValue = 255 - value; // 公共正极反相
  ledcWrite(pin, pwmValue);
}

void allOff() {
  writeLed(RED_PIN, 0);
  writeLed(YELLOW_PIN, 0);
  writeLed(GREEN_PIN, 0);
}

void setOnly(int red, int yellow, int green) {
  writeLed(RED_PIN, constrain(red, 0, RED_MAX));
  writeLed(YELLOW_PIN, constrain(yellow, 0, YELLOW_MAX));
  writeLed(GREEN_PIN, constrain(green, 0, GREEN_MAX));
}

int triWave(unsigned long t, unsigned long period, int maxValue) {
  // 生成一个对称三角波，适合做柔和的呼吸/跑马灯亮度曲线。
  unsigned long x = t % period;
  if (x < period / 2) {
    return map(x, 0, period / 2, 0, maxValue);
  } else {
    return map(x, period / 2, period, maxValue, 0);
  }
}

int fadeInOutBrightness(
  unsigned long t,
  unsigned long fadeIn,
  unsigned long hold,
  unsigned long fadeOut,
  unsigned long offTime,
  int maxValue
) {
  unsigned long total = fadeIn + hold + fadeOut + offTime;
  unsigned long x = t % total;

  if (x < fadeIn) {
    return map(x, 0, fadeIn, 0, maxValue);
  }

  x -= fadeIn;
  if (x < hold) {
    return maxValue;
  }

  x -= hold;
  if (x < fadeOut) {
    return map(x, 0, fadeOut, maxValue, 0);
  }

  return 0;
}

void fadeToStatic(int targetRed, int targetYellow, int targetGreen, int fadeMs = 80) {
  // 模式切换到单灯常亮时做一个短暂渐入，避免“啪”一下硬切太突兀。
  allOff();

  int steps = 12;
  int delayPerStep = max(1, fadeMs / steps);

  for (int i = 0; i <= steps; i++) {
    float p = (float)i / steps;
    setOnly(targetRed * p, targetYellow * p, targetGreen * p);
    delay(delayPerStep);
  }
}


// =====================================================
// 模式处理
// =====================================================

bool isValidMode(String mode) {
  // 这里只接受电脑端协议约定的稳定 mode 字符串。
  return (
    mode == "red" ||
    mode == "yellow" ||
    mode == "green" ||
    mode == "busy" ||
    mode == "error" ||
    mode == "thinking" ||
    mode == "ai" ||
    mode == "success" ||
    mode == "traffic" ||
    mode == "alarm" ||
    mode == "demo" ||
    mode == "off"
  );
}

void notifyMode() {
  // characteristic 同时支持 read + notify：
  // 1. read 便于手机 BLE 工具或调试脚本查询当前模式；
  // 2. notify 便于连接中的 central 感知固件状态变化。
  if (pModeCharacteristic) {
    pModeCharacteristic->setValue(currentMode.c_str());
    if (deviceConnected) {
      pModeCharacteristic->notify();
    }
  }
}

void setMode(String mode) {
  mode.trim();
  mode.toLowerCase();

  if (!isValidMode(mode)) {
    Serial.print("Unknown mode: ");
    Serial.println(mode);
    return;
  }

  currentMode = mode;
  // 每次模式切换都重置超时起点，用于后续自动降级。
  modeStart = millis();

  Serial.print("Mode changed to: ");
  Serial.println(currentMode);

  if (mode == "red") {
    fadeToStatic(RED_MAX, 0, 0, 80);
  } else if (mode == "yellow") {
    fadeToStatic(0, YELLOW_MAX, 0, 80);
  } else if (mode == "green") {
    fadeToStatic(0, 0, GREEN_MAX, 80);
  } else if (mode == "success") {
    setOnly(0, 0, GREEN_MAX);
  } else if (mode == "off") {
    allOff();
  }

  notifyMode();
}

void autoTimeoutCheck() {
  // 固件侧自动超时是最后一道保险：
  // 即使电脑端失联，也不会让灯无限期保持高亮状态。
  unsigned long elapsed = millis() - modeStart;

  if (currentMode == "off") {
    return;
  }

  if (currentMode == "traffic") {
    if (elapsed >= TRAFFIC_MODE_TIMEOUT_MS) {
      Serial.println("Traffic timeout -> off");
      setMode("off");
    }
    return;
  }

  if (elapsed >= NORMAL_MODE_TIMEOUT_MS) {
    Serial.println("Normal mode timeout -> traffic");
    setMode("traffic");
  }
}


// =====================================================
// 灯效模式
// =====================================================

void updateBusy() {
  // 黄灯慢闪，表示“正在执行命令或等待命令完成”。
  unsigned long t = millis() - modeStart;
  int y = fadeInOutBrightness(t, 80, 500, 120, 500, YELLOW_MAX);
  setOnly(0, y, 0);
}

void updateError() {
  // 红灯快闪，强调失败感知。
  unsigned long t = millis() - modeStart;
  int r = fadeInOutBrightness(t, 40, 180, 80, 180, RED_MAX);
  setOnly(r, 0, 0);
}

// thinking：连贯跑马灯，按实物从上到下：L1绿 -> L2黄 -> L3红
void updateThinking() {
  unsigned long t = millis() - modeStart;
  const unsigned long period = 1050;
  unsigned long x = t % period;

  int g = 0;
  int y = 0;
  int r = 0;

  if (x < 350) {
    // 绿 -> 黄 过渡
    g = map(x, 0, 350, GREEN_MAX, 70);
    y = map(x, 0, 350, 20, YELLOW_MAX);
    r = 0;
  } else if (x < 700) {
    // 黄 -> 红 过渡
    unsigned long p = x - 350;
    g = map(p, 0, 350, 70, 0);
    y = map(p, 0, 350, YELLOW_MAX, 70);
    r = map(p, 0, 350, 20, RED_MAX);
  } else {
    // 红 -> 绿 回环，形成连续跑马灯
    unsigned long p = x - 700;
    g = map(p, 0, 350, 20, GREEN_MAX);
    y = map(p, 0, 350, 70, 0);
    r = map(p, 0, 350, RED_MAX, 70);
  }

  setOnly(r, y, g);
}

// ai：柔和版跑马灯，比 thinking 更慢、更柔和、亮度低一点
void updateAi() {
  // AI 生成态比 thinking 更柔和，因此周期更长，峰值亮度也更低。
  unsigned long t = millis() - modeStart;
  const unsigned long period = 1800;
  unsigned long x = t % period;

  unsigned long gx = (x + 0) % period;
  unsigned long yx = (x + period / 3) % period;
  unsigned long rx = (x + 2 * period / 3) % period;

  int g = triWave(gx, period, 150);
  int y = triWave(yx, period, 140);
  int r = triWave(rx, period, 170);

  setOnly(r, y, g);
}

void updateSuccess() {
  // 成功态保持稳定绿灯，避免闪烁干扰用户确认完成。
  setOnly(0, 0, GREEN_MAX);
}

// alarm：红黄交替警灯，带短渐变
void updateAlarm() {
  // 告警态采用红黄交替，更适合表示“需要用户介入”。
  unsigned long t = millis() - modeStart;
  const unsigned long phaseMs = 260;
  int phase = (t / phaseMs) % 2;
  unsigned long inside = t % phaseMs;

  int brightness;
  if (inside < 60) {
    brightness = map(inside, 0, 60, 0, 255);
  } else if (inside < 180) {
    brightness = 255;
  } else {
    brightness = map(inside, 180, phaseMs, 255, 0);
  }

  if (phase == 0) {
    setOnly(brightness, 0, 0);
  } else {
    setOnly(0, min(brightness, YELLOW_MAX), 0);
  }
}

// traffic：红灯变绿前红闪；绿灯变黄前绿闪
void updateTraffic() {
  // traffic 是所有普通状态的自动降级目标。
  // 这里模拟“红 -> 红闪 -> 绿 -> 绿闪 -> 黄”的循环过渡。
  unsigned long t = (millis() - modeStart) % 15000;

  if (t < 5000) {
    setOnly(RED_MAX, 0, 0);
  }

  else if (t < 6500) {
    unsigned long phase = (t - 5000) % 500;
    int r = 0;
    if (phase < 60) r = map(phase, 0, 60, 0, RED_MAX);
    else if (phase < 230) r = RED_MAX;
    else if (phase < 320) r = map(phase, 230, 320, RED_MAX, 0);
    else r = 0;
    setOnly(r, 0, 0);
  }

  else if (t < 11500) {
    setOnly(0, 0, GREEN_MAX);
  }

  else if (t < 13000) {
    unsigned long phase = (t - 11500) % 500;
    int g = 0;
    if (phase < 60) g = map(phase, 0, 60, 0, GREEN_MAX);
    else if (phase < 230) g = GREEN_MAX;
    else if (phase < 320) g = map(phase, 230, 320, GREEN_MAX, 0);
    else g = 0;
    setOnly(0, 0, g);
  }

  else {
    setOnly(0, YELLOW_MAX, 0);
  }
}

// demo：默认开机演示模式
void updateDemo() {
  // demo 只在开机或没有任何状态时使用，用来快速展示所有核心灯效。
  unsigned long t = (millis() - modeStart) % 16000;

  if (t < 1200) {
    int g = triWave(t, 1200, GREEN_MAX);
    setOnly(0, 0, g);
  } else if (t < 2400) {
    int y = triWave(t - 1200, 1200, YELLOW_MAX);
    setOnly(0, y, 0);
  } else if (t < 3600) {
    int r = triWave(t - 2400, 1200, RED_MAX);
    setOnly(r, 0, 0);
  } else if (t < 6200) {
    updateAi();
  } else if (t < 8200) {
    updateThinking();
  } else if (t < 10200) {
    updateBusy();
  } else if (t < 12200) {
    updateError();
  } else if (t < 14200) {
    updateAlarm();
  } else {
    unsigned long p = t - 14200;
    if (p < 600) setOnly(RED_MAX, 0, 0);
    else if (p < 1200) setOnly(0, 0, GREEN_MAX);
    else setOnly(0, YELLOW_MAX, 0);
  }
}


// =====================================================
// BLE 回调
// =====================================================

class ServerCallbacks : public BLEServerCallbacks {
  void onConnect(BLEServer* pServer) {
    // 当前只需要知道“有没有 central 连上”，不区分具体客户端。
    deviceConnected = true;
    Serial.println("BLE client connected.");
  }

  void onDisconnect(BLEServer* pServer) {
    deviceConnected = false;
    Serial.println("BLE client disconnected. Restart advertising.");
    // 断开后立刻重新开始广播，方便 daemon 重连。
    BLEDevice::startAdvertising();
  }
};

class ModeCharacteristicCallbacks : public BLECharacteristicCallbacks {
  void onWrite(BLECharacteristic* pCharacteristic) {
    // central 写入的是 UTF-8 mode 字符串。
    String value = pCharacteristic->getValue();
    value.trim();

    Serial.print("BLE write: ");
    Serial.println(value);

    setMode(value);
  }

  void onRead(BLECharacteristic* pCharacteristic) {
    // read 时始终返回当前 mode，保持协议简单稳定。
    pCharacteristic->setValue(currentMode.c_str());
  }
};


// =====================================================
// 初始化
// =====================================================

void setup() {
  Serial.begin(115200);
  delay(500);

  ledcAttach(RED_PIN, PWM_FREQ, PWM_RESOLUTION);
  ledcAttach(YELLOW_PIN, PWM_FREQ, PWM_RESOLUTION);
  ledcAttach(GREEN_PIN, PWM_FREQ, PWM_RESOLUTION);

  allOff();

  currentMode = "demo";
  modeStart = millis();

  Serial.println();
  Serial.println("Power on. Default mode: demo");
  Serial.println("Common anode BLE enhanced version.");
  Serial.print("BLE device name: ");
  Serial.println(BLE_DEVICE_NAME);

  BLEDevice::init(BLE_DEVICE_NAME);

  pServer = BLEDevice::createServer();
  pServer->setCallbacks(new ServerCallbacks());

  BLEService* pService = pServer->createService(SERVICE_UUID);

  pModeCharacteristic = pService->createCharacteristic(
    MODE_CHAR_UUID,
    BLECharacteristic::PROPERTY_READ |
    BLECharacteristic::PROPERTY_WRITE |
    BLECharacteristic::PROPERTY_NOTIFY
  );

  pModeCharacteristic->setCallbacks(new ModeCharacteristicCallbacks());
  pModeCharacteristic->setValue(currentMode.c_str());
  pModeCharacteristic->addDescriptor(new BLE2902());
  // BLE2902 让 notify 能被常见客户端正确订阅。

  pService->start();

  BLEAdvertising* pAdvertising = BLEDevice::getAdvertising();
  pAdvertising->addServiceUUID(SERVICE_UUID);
  pAdvertising->setScanResponse(true);
  pAdvertising->setMinPreferred(0x06);
  pAdvertising->setMinPreferred(0x12);

  BLEDevice::startAdvertising();

  Serial.println("BLE advertising started.");
  Serial.println("Supported modes:");
  Serial.println("demo / thinking / ai / busy / success / error / alarm / traffic / off / red / yellow / green");
}


// =====================================================
// 主循环
// =====================================================

void loop() {
  // 主循环只有两件事：检查自动超时，按当前模式刷新灯效。
  autoTimeoutCheck();

  if (currentMode == "busy") {
    updateBusy();
  } else if (currentMode == "error") {
    updateError();
  } else if (currentMode == "thinking") {
    updateThinking();
  } else if (currentMode == "ai") {
    updateAi();
  } else if (currentMode == "success") {
    updateSuccess();
  } else if (currentMode == "traffic") {
    updateTraffic();
  } else if (currentMode == "alarm") {
    updateAlarm();
  } else if (currentMode == "demo") {
    updateDemo();
  } else if (currentMode == "off") {
    allOff();
  }

  delay(5);
}
