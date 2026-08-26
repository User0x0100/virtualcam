// SPDX-License-Identifier: MIT
//
// Managed NV12 DirectShow virtual camera built around OBS/libdshowcapture's
// OutputFilter infrastructure. The DirectShow support files under third_party/
// retain their upstream LGPL-2.1-or-later license.

#include "third_party/libdshowcapture/source/output-filter.hpp"
#include "third_party/libdshowcapture/source/dshow-formats.hpp"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstring>
#include <limits>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <strsafe.h>

using namespace DShow;

namespace {

constexpr wchar_t kRegistryRoot[] = L"SOFTWARE\\VirtualCamRs\\ManagedCameras";
constexpr wchar_t kManagementMutex[] = L"Global\\VirtualCamRs-ManagedCamera-Management";
constexpr wchar_t kThreadingModel[] = L"Both";
constexpr wchar_t kValueName[] = L"Name";
constexpr wchar_t kValueQueueName[] = L"QueueName";
constexpr size_t kMaxCameraName = 255;
constexpr size_t kFrameHeaderSize = 32;
constexpr size_t kSlotCount = 3;
constexpr DWORD kRuntimeReadyTimeoutMs = 5000;
constexpr uint32_t kQueueRetryDelayMs = 500;
constexpr uint32_t kProducerLivenessProbeFrames = 10;
constexpr uint32_t kControlMagic = 0x524D4356; // little-endian "VCMR"
constexpr uint32_t kControlVersion = 1;
constexpr uint32_t kControlStateStarting = 1;
constexpr uint32_t kControlStateReady = 2;
constexpr uint32_t kControlStateStopping = 3;
constexpr uint32_t kFallbackWidth = 640;
constexpr uint32_t kFallbackHeight = 480;
constexpr uint64_t kFallbackInterval100ns = 10000000ULL / 30ULL;

HINSTANCE g_module = nullptr;
std::atomic<long> g_locks{0};

struct CameraInfoAbi {
    GUID clsid;
    wchar_t name[256];
    uint32_t reserved_width;
    uint32_t reserved_height;
    uint32_t reserved_fps_numerator;
    uint32_t reserved_fps_denominator;
};

using EnumCallback = BOOL(WINAPI *)(const CameraInfoAbi *info, void *context);

struct CameraConfig {
    GUID clsid{};
    std::wstring name;
    std::wstring queue_name;
};

struct RuntimeFormat {
    uint32_t width = 0;
    uint32_t height = 0;
    uint64_t interval_100ns = 0;
};

#pragma pack(push, 8)
struct ManagedControlHeader {
    uint32_t magic;
    uint32_t version;
    volatile uint32_t state;
    uint32_t reserved;
    uint32_t width;
    uint32_t height;
    uint64_t interval_100ns;
};
#pragma pack(pop)

static_assert(sizeof(ManagedControlHeader) == 32, "managed control ABI changed");

RuntimeFormat FallbackRuntimeFormat()
{
    return RuntimeFormat{kFallbackWidth, kFallbackHeight, kFallbackInterval100ns};
}

#pragma pack(push, 8)
struct QueueHeader {
    volatile uint32_t write_idx;
    volatile uint32_t read_idx;
    volatile uint32_t state;
    uint32_t offsets[kSlotCount];
    uint32_t type;
    uint32_t cx;
    uint32_t cy;
    uint64_t interval;
    uint32_t reserved[8];
};
#pragma pack(pop)

static_assert(sizeof(QueueHeader) == 80, "OBS queue header ABI changed");

constexpr uint32_t kQueueStateStarting = 1;
constexpr uint32_t kQueueStateReady = 2;
constexpr uint32_t kQueueStateStopping = 3;

HRESULT HrFromWin32(LSTATUS status)
{
    return status == ERROR_SUCCESS ? S_OK : HRESULT_FROM_WIN32(static_cast<DWORD>(status));
}

class ManagementLock {
public:
    ManagementLock()
    {
        mutex_ = CreateMutexW(nullptr, FALSE, kManagementMutex);
        if (!mutex_) {
            hr_ = HRESULT_FROM_WIN32(GetLastError());
            return;
        }
        const DWORD wait = WaitForSingleObject(mutex_, INFINITE);
        if (wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED) {
            locked_ = true;
        } else if (wait == WAIT_FAILED) {
            hr_ = HRESULT_FROM_WIN32(GetLastError());
        } else {
            hr_ = E_FAIL;
        }
    }

    ~ManagementLock()
    {
        if (locked_) {
            ReleaseMutex(mutex_);
        }
        if (mutex_) {
            CloseHandle(mutex_);
        }
    }

    HRESULT status() const { return hr_; }

private:
    HANDLE mutex_ = nullptr;
    bool locked_ = false;
    HRESULT hr_ = S_OK;
};

HRESULT FinalPath(const wchar_t *path, std::wstring *out)
{
    if (!path || !out) return E_POINTER;
    HANDLE handle = CreateFileW(path, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr, OPEN_EXISTING,
                                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (handle == INVALID_HANDLE_VALUE) {
        return HRESULT_FROM_WIN32(GetLastError());
    }
    const DWORD required = GetFinalPathNameByHandleW(handle, nullptr, 0, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    if (required == 0) {
        const HRESULT hr = HRESULT_FROM_WIN32(GetLastError());
        CloseHandle(handle);
        return hr;
    }
    std::vector<wchar_t> buffer(static_cast<size_t>(required) + 1);
    const DWORD written = GetFinalPathNameByHandleW(handle, buffer.data(), static_cast<DWORD>(buffer.size()), FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    const HRESULT hr = written == 0 || written >= buffer.size() ? HRESULT_FROM_WIN32(GetLastError()) : S_OK;
    CloseHandle(handle);
    if (FAILED(hr)) return hr;
    out->assign(buffer.data(), written);
    return S_OK;
}

HRESULT RegistryPathFromFinalPath(const std::wstring &final_path, std::wstring *out)
{
    if (!out) return E_POINTER;
    constexpr wchar_t kExtendedPrefix[] = L"\\\\?\\";
    constexpr wchar_t kExtendedUncPrefix[] = L"\\\\?\\UNC\\";
    if (final_path.rfind(kExtendedUncPrefix, 0) == 0) {
        *out = L"\\\\" + final_path.substr(std::size(kExtendedUncPrefix) - 1);
        return S_OK;
    }
    if (final_path.rfind(kExtendedPrefix, 0) == 0) {
        const std::wstring normal = final_path.substr(std::size(kExtendedPrefix) - 1);
        // VOLUME_NAME_DOS normally yields a drive-letter path here. Do not
        // persist volume-GUID/device syntax into COM registration if Windows
        // cannot express the final module as an ordinary DOS path.
        if (normal.size() < 3 || normal[1] != L':' || normal[2] != L'\\') {
            return HRESULT_FROM_WIN32(ERROR_BAD_PATHNAME);
        }
        *out = normal;
        return S_OK;
    }
    *out = final_path;
    return S_OK;
}

HRESULT ModuleRegistryPath(std::wstring *out)
{
    if (!out) return E_POINTER;
    std::vector<wchar_t> module_buffer(32768);
    const DWORD module_len = GetModuleFileNameW(g_module, module_buffer.data(), static_cast<DWORD>(module_buffer.size()));
    if (module_len == 0) return HRESULT_FROM_WIN32(GetLastError());
    if (module_len >= module_buffer.size() - 1) return HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER);

    // Resolve the loaded module through a file handle before persisting it. This
    // removes relative paths, junction aliases and extended-length spelling from
    // the registration while allowing the managed DLL to live beside a portable
    // application instead of requiring a Program Files deployment.
    std::wstring module_final_path;
    HRESULT hr = FinalPath(module_buffer.data(), &module_final_path);
    if (FAILED(hr)) return hr;

    // Machine-wide COM registration may be app-local, but never point at a
    // network share. A mutable remote DLL is not an acceptable in-process COM
    // server target. VOLUME_NAME_DOS resolves network files as \\?\UNC\... .
    constexpr wchar_t kExtendedPrefix[] = L"\\\\?\\";
    constexpr wchar_t kExtendedUncPrefix[] = L"\\\\?\\UNC\\";
    const bool extended_unc = module_final_path.rfind(kExtendedUncPrefix, 0) == 0;
    const bool plain_unc = module_final_path.rfind(kExtendedPrefix, 0) != 0 && module_final_path.rfind(L"\\\\", 0) == 0;
    if (extended_unc || plain_unc) {
        return HRESULT_FROM_WIN32(ERROR_BAD_NETPATH);
    }

    // COM registration receives an ordinary local DOS full path because some
    // activation paths do not reliably accept the \\?\ namespace form.
    return RegistryPathFromFinalPath(module_final_path, out);
}

std::wstring GuidString(REFGUID guid)
{
    wchar_t buffer[64]{};
    if (StringFromGUID2(guid, buffer, static_cast<int>(std::size(buffer))) <= 0) {
        return {};
    }
    return buffer;
}

std::wstring CameraRegistryPath(REFGUID guid)
{
    const auto guid_string = GuidString(guid);
    if (guid_string.empty()) {
        return {};
    }
    return std::wstring(kRegistryRoot) + L"\\" + guid_string;
}

std::wstring QueueName(REFGUID guid)
{
    const auto guid_string = GuidString(guid);
    if (guid_string.empty()) {
        return {};
    }
    return std::wstring(L"Local\\VirtualCamRs-") + guid_string;
}

std::wstring ReadyEventName(const std::wstring &queue_name)
{
    return queue_name.empty() ? std::wstring{} : queue_name + L"-Ready";
}

std::wstring ProducerEventName(const std::wstring &queue_name)
{
    return queue_name.empty() ? std::wstring{} : queue_name + L"-Producer";
}

std::wstring ControlMappingName(const std::wstring &queue_name)
{
    return queue_name.empty() ? std::wstring{} : queue_name + L"-Control";
}

std::wstring DataMappingName(const std::wstring &queue_name, const RuntimeFormat &format)
{
    if (queue_name.empty()) return {};
    return queue_name + L"-Data-" + std::to_wstring(format.width) + L"x" + std::to_wstring(format.height) + L"-" + std::to_wstring(format.interval_100ns);
}

bool ValidDimensions(uint32_t width, uint32_t height)
{
    if (width == 0 || height == 0 || (width % 2) != 0 || (height % 2) != 0 || width > INT_MAX || height > INT_MAX) {
        return false;
    }
    const uint64_t pixels = static_cast<uint64_t>(width) * height;
    // libdshowcapture's pinned CreateMediaType computes cx * cy * bits in
    // signed-int arithmetic before storing the result, so keep that operation
    // representable for the sole 12-bpp NV12 media type we expose.
    return pixels <= static_cast<uint64_t>(INT_MAX) / 12ULL;
}

HRESULT SetString(HKEY key, const wchar_t *name, const std::wstring &value)
{
    const DWORD bytes = static_cast<DWORD>((value.size() + 1) * sizeof(wchar_t));
    return HrFromWin32(RegSetValueExW(key, name, 0, REG_SZ, reinterpret_cast<const BYTE *>(value.c_str()), bytes));
}

HRESULT QueryString(HKEY key, const wchar_t *name, std::wstring *out)
{
    if (!out) {
        return E_POINTER;
    }
    DWORD type = 0;
    DWORD bytes = 0;
    auto status = RegQueryValueExW(key, name, nullptr, &type, nullptr, &bytes);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }
    if (type != REG_SZ || bytes < sizeof(wchar_t) || (bytes % sizeof(wchar_t)) != 0) {
        return HRESULT_FROM_WIN32(ERROR_INVALID_DATA);
    }
    std::vector<wchar_t> buffer(bytes / sizeof(wchar_t));
    status = RegQueryValueExW(key, name, nullptr, &type, reinterpret_cast<BYTE *>(buffer.data()), &bytes);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }
    buffer.back() = L'\0';
    *out = buffer.data();
    return S_OK;
}

HRESULT SaveConfig(const CameraConfig &config)
{
    const auto path = CameraRegistryPath(config.clsid);
    if (path.empty()) {
        return E_INVALIDARG;
    }
    HKEY key = nullptr;
    DWORD disposition = 0;
    auto status = RegCreateKeyExW(HKEY_LOCAL_MACHINE, path.c_str(), 0, nullptr, 0, KEY_READ | KEY_WRITE, nullptr, &key, &disposition);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }

    HRESULT hr = SetString(key, kValueName, config.name);
    if (SUCCEEDED(hr)) hr = SetString(key, kValueQueueName, config.queue_name);
    RegCloseKey(key);
    return hr;
}

HRESULT LoadConfig(REFGUID clsid, CameraConfig *config)
{
    if (!config) {
        return E_POINTER;
    }
    const auto path = CameraRegistryPath(clsid);
    if (path.empty()) {
        return E_INVALIDARG;
    }
    HKEY key = nullptr;
    auto status = RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.c_str(), 0, KEY_READ, &key);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }

    CameraConfig value{};
    value.clsid = clsid;
    HRESULT hr = QueryString(key, kValueName, &value.name);
    if (SUCCEEDED(hr)) hr = QueryString(key, kValueQueueName, &value.queue_name);
    RegCloseKey(key);

    if (SUCCEEDED(hr) && (value.name.empty() || value.queue_name.empty())) {
        hr = HRESULT_FROM_WIN32(ERROR_INVALID_DATA);
    }
    if (SUCCEEDED(hr)) {
        *config = std::move(value);
    }
    return hr;
}

HRESULT DeleteConfig(REFGUID clsid)
{
    const auto path = CameraRegistryPath(clsid);
    if (path.empty()) return E_INVALIDARG;
    const LSTATUS status = RegDeleteTreeW(HKEY_LOCAL_MACHINE, path.c_str());
    if (status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND) return S_OK;
    return HrFromWin32(status);
}

HRESULT RegisterComServerAtPath(REFGUID clsid, const std::wstring &name, const std::wstring &file)
{
    if (file.empty()) return E_INVALIDARG;
    const auto guid_string = GuidString(clsid);
    if (guid_string.empty()) {
        return E_INVALIDARG;
    }
    const std::wstring class_path = L"CLSID\\" + guid_string;
    HKEY class_key = nullptr;
    auto status = RegCreateKeyExW(HKEY_CLASSES_ROOT, class_path.c_str(), 0, nullptr, 0, KEY_READ | KEY_WRITE, nullptr, &class_key, nullptr);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }
    HRESULT hr = SetString(class_key, nullptr, name);
    if (SUCCEEDED(hr)) {
        HKEY server_key = nullptr;
        status = RegCreateKeyExW(class_key, L"InprocServer32", 0, nullptr, 0, KEY_READ | KEY_WRITE, nullptr, &server_key, nullptr);
        if (status == ERROR_SUCCESS) {
            hr = SetString(server_key, nullptr, file);
            if (SUCCEEDED(hr)) hr = SetString(server_key, L"ThreadingModel", kThreadingModel);
            RegCloseKey(server_key);
        } else {
            hr = HrFromWin32(status);
        }
    }
    RegCloseKey(class_key);
    return hr;
}

HRESULT ReadComServerPath(REFGUID clsid, std::wstring *file)
{
    if (!file) return E_POINTER;
    const auto guid_string = GuidString(clsid);
    if (guid_string.empty()) return E_INVALIDARG;
    const std::wstring path = L"CLSID\\" + guid_string + L"\\InprocServer32";
    HKEY key = nullptr;
    const auto status = RegOpenKeyExW(HKEY_CLASSES_ROOT, path.c_str(), 0, KEY_READ, &key);
    if (status != ERROR_SUCCESS) return HrFromWin32(status);
    const HRESULT hr = QueryString(key, nullptr, file);
    RegCloseKey(key);
    return hr;
}

HRESULT UnregisterComServer(REFGUID clsid)
{
    const auto guid_string = GuidString(clsid);
    if (guid_string.empty()) return E_INVALIDARG;
    const std::wstring class_path = L"CLSID\\" + guid_string;
    const LSTATUS status = RegDeleteTreeW(HKEY_CLASSES_ROOT, class_path.c_str());
    if (status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND) return S_OK;
    return HrFromWin32(status);
}

static const REGPINTYPES kMediaType = {&MEDIATYPE_Video, &MEDIASUBTYPE_NV12};
static const REGFILTERPINS kVideoPin = {nullptr, FALSE, TRUE, FALSE, FALSE, &CLSID_NULL, nullptr, 1, &kMediaType};

HRESULT WithFilterMapper(const std::function<HRESULT(IFilterMapper2 *)> &action)
{
    const HRESULT init_hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    const bool uninitialize = SUCCEEDED(init_hr);
    if (FAILED(init_hr) && init_hr != RPC_E_CHANGED_MODE) {
        return init_hr;
    }

    IFilterMapper2 *mapper = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_FilterMapper2, nullptr, CLSCTX_INPROC_SERVER, IID_IFilterMapper2, reinterpret_cast<void **>(&mapper));
    if (SUCCEEDED(hr)) {
        hr = action(mapper);
        mapper->Release();
    }
    if (uninitialize) {
        CoUninitialize();
    }
    return hr;
}

HRESULT RegisterFilter(REFGUID clsid, const std::wstring &name)
{
    REGFILTER2 filter{};
    filter.dwVersion = 1;
    filter.dwMerit = MERIT_DO_NOT_USE;
    filter.cPins = 1;
    filter.rgPins = &kVideoPin;
    return WithFilterMapper([&](IFilterMapper2 *mapper) {
        return mapper->RegisterFilter(clsid, name.c_str(), nullptr, &CLSID_VideoInputDeviceCategory, nullptr, &filter);
    });
}

HRESULT UnregisterFilter(REFGUID clsid)
{
    return WithFilterMapper([&](IFilterMapper2 *mapper) {
        const HRESULT hr = mapper->UnregisterFilter(&CLSID_VideoInputDeviceCategory, nullptr, clsid);
        return hr == HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) ? S_OK : hr;
    });
}

HRESULT WriteClassName(REFGUID clsid, const std::wstring &name)
{
    const auto guid_string = GuidString(clsid);
    if (guid_string.empty()) {
        return E_INVALIDARG;
    }
    const std::wstring path = L"CLSID\\" + guid_string;
    HKEY key = nullptr;
    const auto status = RegOpenKeyExW(HKEY_CLASSES_ROOT, path.c_str(), 0, KEY_SET_VALUE, &key);
    if (status != ERROR_SUCCESS) {
        return HrFromWin32(status);
    }
    const HRESULT hr = SetString(key, nullptr, name);
    RegCloseKey(key);
    return hr;
}

HRESULT FillInfo(const CameraConfig &config, CameraInfoAbi *info)
{
    if (!info) {
        return E_POINTER;
    }
    CameraInfoAbi value{};
    value.clsid = config.clsid;
    const HRESULT hr = StringCchCopyW(value.name, std::size(value.name), config.name.c_str());
    if (FAILED(hr)) {
        return hr;
    }
    *info = value;
    return S_OK;
}

HRESULT LoadRuntimeFormat(const CameraConfig &config, RuntimeFormat *format)
{
    if (!format) return E_POINTER;

    const auto producer_name = ProducerEventName(config.queue_name);
    if (producer_name.empty()) return E_INVALIDARG;
    HANDLE producer_event = OpenEventW(SYNCHRONIZE, FALSE, producer_name.c_str());
    if (!producer_event) {
        const DWORD error = GetLastError();
        return HRESULT_FROM_WIN32(error == ERROR_FILE_NOT_FOUND ? ERROR_NOT_READY : error);
    }
    // The event exists immediately to reserve the single producer, but is only
    // signaled after Ready/control/data publication has completed.
    const DWORD producer_wait = WaitForSingleObject(producer_event, kRuntimeReadyTimeoutMs);
    if (producer_wait != WAIT_OBJECT_0) {
        const HRESULT wait_hr = producer_wait == WAIT_TIMEOUT ? HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                                                             : producer_wait == WAIT_FAILED ? HRESULT_FROM_WIN32(GetLastError()) : E_FAIL;
        CloseHandle(producer_event);
        return wait_hr;
    }
    CloseHandle(producer_event);

    const auto ready_name = ReadyEventName(config.queue_name);
    if (ready_name.empty()) return E_INVALIDARG;
    HANDLE ready_event = OpenEventW(SYNCHRONIZE, FALSE, ready_name.c_str());
    if (!ready_event) {
        return HRESULT_FROM_WIN32(GetLastError());
    }
    const DWORD ready_wait = WaitForSingleObject(ready_event, kRuntimeReadyTimeoutMs);
    if (ready_wait != WAIT_OBJECT_0) {
        const HRESULT wait_hr = ready_wait == WAIT_TIMEOUT ? HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                                                          : ready_wait == WAIT_FAILED ? HRESULT_FROM_WIN32(GetLastError()) : E_FAIL;
        CloseHandle(ready_event);
        return wait_hr;
    }
    CloseHandle(ready_event);

    const auto control_name = ControlMappingName(config.queue_name);
    if (control_name.empty()) return E_INVALIDARG;
    HANDLE mapping = OpenFileMappingW(FILE_MAP_READ, FALSE, control_name.c_str());
    if (!mapping) {
        return HRESULT_FROM_WIN32(GetLastError());
    }
    auto *view = static_cast<uint8_t *>(MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0));
    if (!view) {
        const HRESULT hr = HRESULT_FROM_WIN32(GetLastError());
        CloseHandle(mapping);
        return hr;
    }

    MEMORY_BASIC_INFORMATION info{};
    HRESULT hr = S_OK;
    if (VirtualQuery(view, &info, sizeof(info)) == 0 || info.RegionSize < sizeof(ManagedControlHeader)) {
        hr = HRESULT_FROM_WIN32(ERROR_INVALID_DATA);
    } else {
        auto *control = reinterpret_cast<ManagedControlHeader *>(view);
        MemoryBarrier();
        const uint32_t state = control->state;
        RuntimeFormat value{};
        value.width = control->width;
        value.height = control->height;
        value.interval_100ns = control->interval_100ns;
        if (control->magic != kControlMagic || control->version != kControlVersion ||
            (state != kControlStateStarting && state != kControlStateReady) || !ValidDimensions(value.width, value.height) ||
            value.interval_100ns == 0 || value.interval_100ns > static_cast<uint64_t>((std::numeric_limits<long long>::max)()) ||
            value.interval_100ns > (std::numeric_limits<uint64_t>::max)() / 100ULL) {
            hr = HRESULT_FROM_WIN32(ERROR_INVALID_DATA);
        } else {
            *format = value;
        }
    }

    UnmapViewOfFile(view);
    CloseHandle(mapping);
    return hr;
}

class QueueReader {
public:
    QueueReader(std::wstring name, uint32_t width, uint32_t height, uint64_t interval)
        : name_(std::move(name)), producer_name_(ProducerEventName(name_)), control_name_(ControlMappingName(name_)), width_(width), height_(height), interval_(interval),
          frame_size_(static_cast<size_t>(width) * height * 3 / 2)
    {
        data_name_ = DataMappingName(name_, RuntimeFormat{width_, height_, interval_});
    }

    ~QueueReader() { Close(); }

    bool Read(uint8_t *destination)
    {
        if (!header_) {
            const auto now = std::chrono::steady_clock::now();
            if (now < retry_after_) {
                return false;
            }
            if (!Open()) {
                ScheduleRetry(now);
                return false;
            }
        }
        if (header_->state == kQueueStateStopping) {
            CloseAndRetry();
            return false;
        }
        if (header_->state != kQueueStateReady) {
            ProbeProducerLiveness();
            return false;
        }
        if (header_->cx != width_ || header_->cy != height_ || header_->interval != interval_) {
            // A connected DirectShow graph cannot change mode in place. Release
            // the incompatible producer mapping, then poll at a low frequency so
            // a consumer-first fallback graph does not churn kernel objects every
            // frame while waiting for a new matching producer session.
            CloseAndRetry();
            return false;
        }

        MemoryBarrier();
        const uint32_t increment = header_->read_idx;
        if (!has_last_increment_ || increment != last_increment_) {
            has_last_increment_ = true;
            last_increment_ = increment;
            stagnant_frames_ = 0;
        } else if (!ProbeProducerLiveness()) {
            return false;
        }

        const size_t slot = increment % kSlotCount;
        const size_t offset = header_->offsets[slot];
        if (offset > view_size_ || kFrameHeaderSize > view_size_ - offset || frame_size_ > view_size_ - offset - kFrameHeaderSize) {
            CloseAndRetry();
            return false;
        }
        std::memcpy(destination, view_ + offset + kFrameHeaderSize, frame_size_);
        return true;
    }

private:
    void ScheduleRetry(std::chrono::steady_clock::time_point now)
    {
        retry_after_ = now + std::chrono::milliseconds(kQueueRetryDelayMs);
    }

    void CloseAndRetry()
    {
        Close();
        ScheduleRetry(std::chrono::steady_clock::now());
    }

    bool ProducerAlive() const
    {
        if (producer_name_.empty()) return false;
        HANDLE producer = OpenEventW(SYNCHRONIZE, FALSE, producer_name_.c_str());
        if (!producer) {
            // Missing means the producer lease disappeared. For other lookup
            // errors, avoid a false death signal caused by transient ACL/system
            // failures.
            return GetLastError() != ERROR_FILE_NOT_FOUND;
        }
        const DWORD wait = WaitForSingleObject(producer, 0);
        CloseHandle(producer);
        if (wait == WAIT_OBJECT_0) return true;
        if (wait == WAIT_TIMEOUT) return false;
        return true;
    }

    bool CurrentProducerMatchesFormat() const
    {
        if (control_name_.empty()) return false;
        HANDLE mapping = OpenFileMappingW(FILE_MAP_READ, FALSE, control_name_.c_str());
        if (!mapping) return false;
        auto *view = static_cast<uint8_t *>(MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0));
        if (!view) {
            CloseHandle(mapping);
            return false;
        }
        MEMORY_BASIC_INFORMATION info{};
        bool matches = false;
        if (VirtualQuery(view, &info, sizeof(info)) != 0 && info.RegionSize >= sizeof(ManagedControlHeader)) {
            auto *control = reinterpret_cast<ManagedControlHeader *>(view);
            MemoryBarrier();
            const uint32_t state = control->state;
            matches = control->magic == kControlMagic && control->version == kControlVersion &&
                      (state == kControlStateStarting || state == kControlStateReady) && control->width == width_ &&
                      control->height == height_ && control->interval_100ns == interval_;
        }
        UnmapViewOfFile(view);
        CloseHandle(mapping);
        return matches;
    }

    bool ProbeProducerLiveness()
    {
        if (++stagnant_frames_ < kProducerLivenessProbeFrames) {
            return true;
        }
        stagnant_frames_ = 0;
        if (ProducerAlive() && CurrentProducerMatchesFormat()) {
            return true;
        }
        CloseAndRetry();
        return false;
    }

    bool Open()
    {
        if (!ProducerAlive() || !CurrentProducerMatchesFormat() || data_name_.empty()) {
            return false;
        }
        mapping_ = OpenFileMappingW(FILE_MAP_READ, FALSE, data_name_.c_str());
        if (!mapping_) {
            return false;
        }
        view_ = static_cast<uint8_t *>(MapViewOfFile(mapping_, FILE_MAP_READ, 0, 0, 0));
        if (!view_) {
            Close();
            return false;
        }
        MEMORY_BASIC_INFORMATION info{};
        if (VirtualQuery(view_, &info, sizeof(info)) == 0 || info.RegionSize < sizeof(QueueHeader)) {
            Close();
            return false;
        }
        view_size_ = info.RegionSize;
        header_ = reinterpret_cast<QueueHeader *>(view_);
        return true;
    }

    void Close()
    {
        header_ = nullptr;
        view_size_ = 0;
        has_last_increment_ = false;
        last_increment_ = 0;
        stagnant_frames_ = 0;
        if (view_) {
            UnmapViewOfFile(view_);
            view_ = nullptr;
        }
        if (mapping_) {
            CloseHandle(mapping_);
            mapping_ = nullptr;
        }
    }

    std::wstring name_;
    std::wstring producer_name_;
    std::wstring control_name_;
    std::wstring data_name_;
    uint32_t width_;
    uint32_t height_;
    uint64_t interval_;
    size_t frame_size_;
    HANDLE mapping_ = nullptr;
    uint8_t *view_ = nullptr;
    QueueHeader *header_ = nullptr;
    size_t view_size_ = 0;
    bool has_last_increment_ = false;
    uint32_t last_increment_ = 0;
    uint32_t stagnant_frames_ = 0;
    std::chrono::steady_clock::time_point retry_after_{};
};

class ManagedFilter final : public OutputFilter {
public:
    ManagedFilter(CameraConfig config, RuntimeFormat format)
        : OutputFilter(VideoFormat::NV12, static_cast<int>(format.width), static_cast<int>(format.height), static_cast<long long>(format.interval_100ns)),
          config_(std::move(config)), format_(format), queue_(config_.queue_name, format_.width, format_.height, format_.interval_100ns)
    {
        g_locks.fetch_add(1, std::memory_order_relaxed);
        AddRef();
        worker_ = std::thread([this] { ThreadMain(); });
    }

    ~ManagedFilter() override
    {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stopping_ = true;
            active_ = false;
        }
        condition_.notify_all();
        if (worker_.joinable()) {
            worker_.join();
        }
        g_locks.fetch_sub(1, std::memory_order_relaxed);
    }

    STDMETHODIMP Pause() override
    {
        const HRESULT hr = OutputFilter::Pause();
        if (FAILED(hr)) {
            return hr;
        }
        SetActive(true);
        return S_OK;
    }

    STDMETHODIMP Run(REFERENCE_TIME start) override
    {
        const HRESULT hr = OutputFilter::Run(start);
        if (FAILED(hr)) {
            return hr;
        }
        SetActive(true);
        return S_OK;
    }

    STDMETHODIMP Stop() override
    {
        SetActive(false);
        return OutputFilter::Stop();
    }

protected:
    const wchar_t *FilterName() const override { return config_.name.c_str(); }

private:
    void SetActive(bool active)
    {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            active_ = active;
        }
        condition_.notify_all();
    }

    uint64_t FilterTime100ns() const
    {
        if (clock) {
            REFERENCE_TIME value = 0;
            if (SUCCEEDED(clock->GetTime(&value)) && value >= 0) {
                return static_cast<uint64_t>(value);
            }
        }
        const auto now = std::chrono::steady_clock::now().time_since_epoch();
        return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(now).count() / 100);
    }

    void ThreadMain()
    {
        std::unique_lock<std::mutex> lock(mutex_);
        while (!stopping_) {
            condition_.wait(lock, [&] { return stopping_ || active_; });
            if (stopping_) {
                break;
            }

            uint64_t sample_time = FilterTime100ns();
            auto next = std::chrono::steady_clock::now();
            const auto frame_interval = std::chrono::nanoseconds(format_.interval_100ns * 100ULL);
            while (active_ && !stopping_) {
                lock.unlock();
                Frame(sample_time);
                lock.lock();

                const auto now = std::chrono::steady_clock::now();
                if (now > next + frame_interval) {
                    // A downstream stall must not turn into a burst of historical
                    // catch-up frames. Drop missed deadlines and resume from the
                    // current clock at the next normal frame boundary.
                    next = now + frame_interval;
                    sample_time = FilterTime100ns() + format_.interval_100ns;
                } else {
                    next += frame_interval;
                    sample_time += format_.interval_100ns;
                }
                condition_.wait_until(lock, next, [&] { return stopping_ || !active_; });
            }
        }
    }

    void Frame(uint64_t timestamp)
    {
        uint8_t *destination = nullptr;
        if (!LockSampleData(&destination)) {
            return;
        }
        if (!queue_.Read(destination)) {
            const size_t y_size = static_cast<size_t>(format_.width) * format_.height;
            std::memset(destination, 16, y_size);
            std::memset(destination + y_size, 128, y_size / 2);
        }
        UnlockSampleData(static_cast<long long>(timestamp), static_cast<long long>(timestamp + format_.interval_100ns));
    }

    CameraConfig config_;
    RuntimeFormat format_;
    QueueReader queue_;
    std::thread worker_;
    std::mutex mutex_;
    std::condition_variable condition_;
    bool active_ = false;
    bool stopping_ = false;
};

class ManagedFactory final : public IClassFactory {
public:
    explicit ManagedFactory(CameraConfig config) : config_(std::move(config))
    {
        g_locks.fetch_add(1, std::memory_order_relaxed);
    }

    ~ManagedFactory() { g_locks.fetch_sub(1, std::memory_order_relaxed); }

    STDMETHODIMP QueryInterface(REFIID iid, void **out) override
    {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (iid == IID_IUnknown || iid == IID_IClassFactory) {
            *out = static_cast<IClassFactory *>(this);
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() override { return static_cast<ULONG>(InterlockedIncrement(&refs_)); }

    STDMETHODIMP_(ULONG) Release() override
    {
        const long refs = InterlockedDecrement(&refs_);
        if (refs == 0) {
            delete this;
            return 0;
        }
        return static_cast<ULONG>(refs);
    }

    STDMETHODIMP CreateInstance(IUnknown *outer, REFIID iid, void **out) override
    {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (outer) return CLASS_E_NOAGGREGATION;

        RuntimeFormat format = FallbackRuntimeFormat();
        // A missing producer is the one intentional idle case. If a producer
        // lease exists but its queue is corrupt, inaccessible, or never becomes
        // ready, surface that real activation error instead of silently masking
        // it as a 640x480 black fallback.
        const HRESULT format_hr = LoadRuntimeFormat(config_, &format);
        if (FAILED(format_hr) && format_hr != HRESULT_FROM_WIN32(ERROR_NOT_READY)) {
            return format_hr;
        }

        auto *filter = new (std::nothrow) ManagedFilter(config_, format);
        if (!filter) return E_OUTOFMEMORY;
        const HRESULT hr = filter->QueryInterface(iid, out);
        filter->Release();
        return hr;
    }

    STDMETHODIMP LockServer(BOOL lock) override
    {
        if (lock) g_locks.fetch_add(1, std::memory_order_relaxed);
        else g_locks.fetch_sub(1, std::memory_order_relaxed);
        return S_OK;
    }

private:
    volatile long refs_ = 1;
    CameraConfig config_;
};

} // namespace

extern "C" __declspec(dllexport) uint32_t WINAPI VirtualCamGetAbiVersion()
{
    return 3;
}

extern "C" __declspec(dllexport) HRESULT WINAPI VirtualCamRegister(const wchar_t *name, uint32_t width, uint32_t height,
                                                                    uint32_t fps_numerator, uint32_t fps_denominator,
                                                                    CameraInfoAbi *out_info)
{
    if (!name || !out_info || name[0] == L'\0' || wcslen(name) > kMaxCameraName) {
        return E_INVALIDARG;
    }
    if (width != 0 || height != 0 || fps_numerator != 0 || fps_denominator != 0) {
        return HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
    }
    std::wstring module_path;
    HRESULT hr = ModuleRegistryPath(&module_path);
    if (FAILED(hr)) return hr;
    ManagementLock lock;
    if (FAILED(lock.status())) return lock.status();

    CameraConfig config{};
    config.name = name;
    hr = CoCreateGuid(&config.clsid);
    if (FAILED(hr)) return hr;
    config.queue_name = QueueName(config.clsid);
    if (config.queue_name.empty()) return E_FAIL;

    auto cleanup = [&]() -> HRESULT {
        HRESULT cleanup_hr = UnregisterFilter(config.clsid);
        const HRESULT config_hr = DeleteConfig(config.clsid);
        if (SUCCEEDED(cleanup_hr) && FAILED(config_hr)) cleanup_hr = config_hr;
        const HRESULT com_hr = UnregisterComServer(config.clsid);
        if (SUCCEEDED(cleanup_hr) && FAILED(com_hr)) cleanup_hr = com_hr;
        return cleanup_hr;
    };

    hr = RegisterComServerAtPath(config.clsid, config.name, module_path);
    if (FAILED(hr)) {
        const HRESULT cleanup_hr = cleanup();
        return FAILED(cleanup_hr) ? cleanup_hr : hr;
    }
    hr = SaveConfig(config);
    if (FAILED(hr)) {
        const HRESULT cleanup_hr = cleanup();
        return FAILED(cleanup_hr) ? cleanup_hr : hr;
    }
    hr = RegisterFilter(config.clsid, config.name);
    if (FAILED(hr)) {
        const HRESULT cleanup_hr = cleanup();
        return FAILED(cleanup_hr) ? cleanup_hr : hr;
    }
    hr = FillInfo(config, out_info);
    if (FAILED(hr)) {
        const HRESULT cleanup_hr = cleanup();
        return FAILED(cleanup_hr) ? cleanup_hr : hr;
    }
    return S_OK;
}

extern "C" __declspec(dllexport) HRESULT WINAPI VirtualCamUnregister(const GUID *clsid)
{
    if (!clsid) return E_POINTER;
    ManagementLock lock;
    if (FAILED(lock.status())) return lock.status();

    CameraConfig config{};
    HRESULT hr = LoadConfig(*clsid, &config);
    if (FAILED(hr)) return hr;
    std::wstring original_module_path;
    hr = ReadComServerPath(*clsid, &original_module_path);
    if (FAILED(hr)) return hr;

    hr = UnregisterFilter(*clsid);
    if (FAILED(hr)) return hr;

    hr = UnregisterComServer(*clsid);
    if (FAILED(hr)) {
        const HRESULT rollback_hr = RegisterFilter(config.clsid, config.name);
        return FAILED(rollback_hr) ? rollback_hr : hr;
    }

    hr = DeleteConfig(*clsid);
    if (FAILED(hr)) {
        HRESULT rollback_hr = RegisterComServerAtPath(config.clsid, config.name, original_module_path);
        if (SUCCEEDED(rollback_hr)) rollback_hr = RegisterFilter(config.clsid, config.name);
        if (SUCCEEDED(rollback_hr)) rollback_hr = SaveConfig(config);
        return FAILED(rollback_hr) ? rollback_hr : hr;
    }
    return S_OK;
}

extern "C" __declspec(dllexport) HRESULT WINAPI VirtualCamRename(const GUID *clsid, const wchar_t *name)
{
    if (!clsid || !name || name[0] == L'\0' || wcslen(name) > kMaxCameraName) return E_INVALIDARG;
    ManagementLock lock;
    if (FAILED(lock.status())) return lock.status();

    CameraConfig old_config{};
    HRESULT hr = LoadConfig(*clsid, &old_config);
    if (FAILED(hr)) return hr;
    std::wstring original_module_path;
    hr = ReadComServerPath(*clsid, &original_module_path);
    if (FAILED(hr)) return hr;
    CameraConfig new_config = old_config;
    new_config.name = name;

    auto rollback = [&]() -> HRESULT {
        UnregisterFilter(old_config.clsid);
        HRESULT rollback_hr = RegisterComServerAtPath(old_config.clsid, old_config.name, original_module_path);
        if (SUCCEEDED(rollback_hr)) rollback_hr = RegisterFilter(old_config.clsid, old_config.name);
        if (SUCCEEDED(rollback_hr)) rollback_hr = SaveConfig(old_config);
        return rollback_hr;
    };

    hr = UnregisterFilter(*clsid);
    if (FAILED(hr)) return hr;
    hr = WriteClassName(*clsid, new_config.name);
    if (FAILED(hr)) {
        const HRESULT rollback_hr = rollback();
        return FAILED(rollback_hr) ? rollback_hr : hr;
    }
    hr = RegisterFilter(*clsid, new_config.name);
    if (FAILED(hr)) {
        const HRESULT rollback_hr = rollback();
        return FAILED(rollback_hr) ? rollback_hr : hr;
    }
    hr = SaveConfig(new_config);
    if (FAILED(hr)) {
        const HRESULT rollback_hr = rollback();
        return FAILED(rollback_hr) ? rollback_hr : hr;
    }
    return S_OK;
}

extern "C" __declspec(dllexport) HRESULT WINAPI VirtualCamGet(const GUID *clsid, CameraInfoAbi *out_info)
{
    if (!clsid || !out_info) return E_POINTER;
    ManagementLock lock;
    if (FAILED(lock.status())) return lock.status();
    CameraConfig config{};
    const HRESULT hr = LoadConfig(*clsid, &config);
    return FAILED(hr) ? hr : FillInfo(config, out_info);
}

extern "C" __declspec(dllexport) HRESULT WINAPI VirtualCamEnumerate(EnumCallback callback, void *context)
{
    if (!callback) return E_POINTER;
    ManagementLock lock;
    if (FAILED(lock.status())) return lock.status();
    HKEY root = nullptr;
    const auto status = RegOpenKeyExW(HKEY_LOCAL_MACHINE, kRegistryRoot, 0, KEY_READ, &root);
    if (status == ERROR_FILE_NOT_FOUND) return S_OK;
    if (status != ERROR_SUCCESS) return HrFromWin32(status);

    DWORD index = 0;
    wchar_t name[64]{};
    while (true) {
        DWORD chars = static_cast<DWORD>(std::size(name));
        FILETIME time{};
        const auto enum_status = RegEnumKeyExW(root, index++, name, &chars, nullptr, nullptr, nullptr, &time);
        if (enum_status == ERROR_NO_MORE_ITEMS) break;
        if (enum_status != ERROR_SUCCESS) {
            RegCloseKey(root);
            return HrFromWin32(enum_status);
        }
        GUID clsid{};
        if (FAILED(CLSIDFromString(name, &clsid))) continue;
        CameraConfig config{};
        if (FAILED(LoadConfig(clsid, &config))) continue;
        CameraInfoAbi info{};
        if (FAILED(FillInfo(config, &info))) continue;
        if (!callback(&info, context)) break;
    }
    RegCloseKey(root);
    return S_OK;
}

STDAPI DllGetClassObject(REFCLSID clsid, REFIID iid, void **out)
{
    if (!out) return E_POINTER;
    *out = nullptr;
    if (iid != IID_IClassFactory && iid != IID_IUnknown) return E_NOINTERFACE;

    CameraConfig config{};
    const HRESULT hr = LoadConfig(clsid, &config);
    if (FAILED(hr)) return CLASS_E_CLASSNOTAVAILABLE;
    auto *factory = new (std::nothrow) ManagedFactory(std::move(config));
    if (!factory) return E_OUTOFMEMORY;
    *out = static_cast<IClassFactory *>(factory);
    return S_OK;
}

STDAPI DllCanUnloadNow()
{
    return g_locks.load(std::memory_order_relaxed) == 0 ? S_OK : S_FALSE;
}

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID)
{
    if (reason == DLL_PROCESS_ATTACH) {
        g_module = module;
        DisableThreadLibraryCalls(module);
    }
    return TRUE;
}
