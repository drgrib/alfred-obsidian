#import <Foundation/Foundation.h>
#import <Vision/Vision.h>
#import <ImageIO/ImageIO.h>

const char* perform_ocr(const char* image_path) {
    @autoreleasepool {
        NSString *path = [NSString stringWithUTF8String:image_path];
        NSURL *url = [NSURL fileURLWithPath:path];
        
        CGImageSourceRef imageSource = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
        if (!imageSource) return strdup("");
        
        CGImageRef cgImage = CGImageSourceCreateImageAtIndex(imageSource, 0, NULL);
        if (!cgImage) {
            CFRelease(imageSource);
            return strdup("");
        }
        
        VNRecognizeTextRequest *request = [[VNRecognizeTextRequest alloc] init];
        request.recognitionLevel = VNRequestTextRecognitionLevelAccurate;
        
        VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCGImage:cgImage options:@{}];
        
        NSError *error = nil;
        [handler performRequests:@[request] error:&error];
        
        CGImageRelease(cgImage);
        CFRelease(imageSource);
        
        if (error || !request.results) return strdup("");
        
        NSMutableString *result = [NSMutableString string];
        for (VNRecognizedTextObservation *observation in request.results) {
            NSArray<VNRecognizedText *> *topCandidates = [observation topCandidates:1];
            if (topCandidates.count > 0) {
                [result appendFormat:@"%@\n", topCandidates.firstObject.string];
            }
        }
        
        return strdup([result UTF8String]);
    }
}

void free_ocr_string(char* str) {
    if (str) {
        free(str);
    }
}