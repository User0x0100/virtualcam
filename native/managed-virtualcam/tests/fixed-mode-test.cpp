#include "../third_party/libdshowcapture/source/output-filter.hpp"
#include "../third_party/libdshowcapture/source/dshow-formats.hpp"
#include "../third_party/libdshowcapture/source/dshow-media-type.hpp"

#include <cstdio>

using namespace DShow;

int main()
{
    OutputFilter filter(VideoFormat::NV12, 320, 240, 333333);
    OutputPin *pin = filter.GetPin();

    AM_MEDIA_TYPE *media_type = nullptr;
    if (FAILED(pin->GetFormat(&media_type)) || !media_type) {
        std::fprintf(stderr, "GetFormat failed\n");
        return 1;
    }

    auto cleanup = [&]() {
        FreeMediaType(*media_type);
        CoTaskMemFree(media_type);
    };

    if (pin->QueryAccept(media_type) != S_OK || FAILED(pin->SetFormat(media_type))) {
        std::fprintf(stderr, "advertised fixed NV12 mode was rejected\n");
        cleanup();
        return 2;
    }

    auto *vih = reinterpret_cast<VIDEOINFOHEADER *>(media_type->pbFormat);
    const LONG original_width = vih->bmiHeader.biWidth;
    vih->bmiHeader.biWidth = original_width / 2;
    if (pin->QueryAccept(media_type) != S_FALSE || pin->SetFormat(media_type) != VFW_E_INVALIDMEDIATYPE) {
        std::fprintf(stderr, "mismatched width was accepted\n");
        cleanup();
        return 3;
    }

    vih->bmiHeader.biWidth = original_width;
    const GUID original_subtype = media_type->subtype;
    media_type->subtype = MEDIASUBTYPE_YUY2;
    if (pin->QueryAccept(media_type) != S_FALSE || pin->SetFormat(media_type) != VFW_E_INVALIDMEDIATYPE) {
        std::fprintf(stderr, "mismatched pixel format was accepted\n");
        cleanup();
        return 4;
    }

    media_type->subtype = original_subtype;
    const REFERENCE_TIME original_interval = vih->AvgTimePerFrame;
    vih->AvgTimePerFrame = original_interval + 1;
    if (pin->QueryAccept(media_type) != S_FALSE || pin->SetFormat(media_type) != VFW_E_INVALIDMEDIATYPE) {
        std::fprintf(stderr, "mismatched frame interval was accepted\n");
        cleanup();
        return 5;
    }
    vih->AvgTimePerFrame = original_interval;
    cleanup();

    const HRESULT init_hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    const bool uninitialize = SUCCEEDED(init_hr);
    if (FAILED(init_hr) && init_hr != RPC_E_CHANGED_MODE) {
        std::fprintf(stderr, "CoInitializeEx failed: 0x%08lx\n", static_cast<unsigned long>(init_hr));
        return 6;
    }

    IGraphBuilder *graph = nullptr;
    ICaptureGraphBuilder2 *capture_builder = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_FilterGraph, nullptr, CLSCTX_INPROC_SERVER, IID_IGraphBuilder, reinterpret_cast<void **>(&graph));
    if (SUCCEEDED(hr)) {
        hr = CoCreateInstance(CLSID_CaptureGraphBuilder2, nullptr, CLSCTX_INPROC_SERVER, IID_ICaptureGraphBuilder2,
                              reinterpret_cast<void **>(&capture_builder));
    }
    if (SUCCEEDED(hr)) {
        hr = capture_builder->SetFiltergraph(graph);
    }

    auto *graph_filter = new (std::nothrow) OutputFilter(VideoFormat::NV12, 640, 480, 333333);
    if (!graph_filter) {
        hr = E_OUTOFMEMORY;
    } else {
        graph_filter->AddRef();
    }
    if (SUCCEEDED(hr)) {
        hr = graph->AddFilter(graph_filter, L"OpenCV compatibility probe");
    }

    IAMStreamConfig *stream_config = nullptr;
    if (SUCCEEDED(hr)) {
        // OpenCV's DirectShow backend asks the capture graph builder for
        // IAMStreamConfig on PIN_CATEGORY_CAPTURE before configuring the stream.
        hr = capture_builder->FindInterface(&PIN_CATEGORY_CAPTURE, &MEDIATYPE_Video, graph_filter, IID_IAMStreamConfig,
                                            reinterpret_cast<void **>(&stream_config));
    }
    if (stream_config) {
        AM_MEDIA_TYPE *probe_type = nullptr;
        if (SUCCEEDED(hr)) {
            hr = stream_config->GetFormat(&probe_type);
        }
        if (probe_type) {
            FreeMediaType(*probe_type);
            CoTaskMemFree(probe_type);
        }
        stream_config->Release();
    }

    if (graph_filter) {
        if (graph) {
            graph->RemoveFilter(graph_filter);
        }
        graph_filter->Release();
    }
    if (capture_builder) capture_builder->Release();
    if (graph) graph->Release();
    if (uninitialize) CoUninitialize();

    if (FAILED(hr)) {
        std::fprintf(stderr, "OpenCV-style capture pin IAMStreamConfig discovery failed: 0x%08lx\n", static_cast<unsigned long>(hr));
        return 7;
    }

    return 0;
}
