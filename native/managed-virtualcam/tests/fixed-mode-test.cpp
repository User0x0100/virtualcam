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

    cleanup();
    return 0;
}
