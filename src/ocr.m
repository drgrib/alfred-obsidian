#import <Foundation/Foundation.h>
#import <Vision/Vision.h>
#import <ImageIO/ImageIO.h>
#import <PDFKit/PDFKit.h>

/// Runs Vision text recognition over one image and returns the recognized text, one
/// line per observation. Returns an empty string when recognition fails.
static NSString* ocr_cgimage(CGImageRef cgImage) {
    if (!cgImage) return @"";
    
    VNRecognizeTextRequest *request = [[VNRecognizeTextRequest alloc] init];
    request.recognitionLevel = VNRequestTextRecognitionLevelAccurate;
    
    VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCGImage:cgImage options:@{}];
    
    NSError *error = nil;
    @try {
        [handler performRequests:@[request] error:&error];
    } @catch (NSException *e) {
        // Vision can raise on a malformed image; treat that as "no text"
        return @"";
    }
    
    if (error || !request.results) return @"";
    
    NSMutableString *result = [NSMutableString string];
    for (VNRecognizedTextObservation *observation in request.results) {
        NSArray<VNRecognizedText *> *topCandidates = [observation topCandidates:1];
        if (topCandidates.count > 0) {
            [result appendFormat:@"%@\n", topCandidates.firstObject.string];
        }
    }
    
    return result;
}

const char* perform_ocr(const char* image_path) {
    @autoreleasepool {
        @try {
            NSString *path = [NSString stringWithUTF8String:image_path];
            if (!path) return strdup("");
            NSURL *url = [NSURL fileURLWithPath:path];
            
            if ([path.pathExtension.lowercaseString isEqualToString:@"pdf"]) {
                PDFDocument *doc = [[PDFDocument alloc] initWithURL:url];
                if (!doc) return strdup("");
                
                // An encrypted document exposes nothing readable without its password
                if (doc.isLocked && ![doc unlockWithPassword:@""]) return strdup("");
                
                // Reading an embedded text layer is instant, while rendering and recognizing a
                // page is not, so only a limited number of image-only pages get the Vision pass
                const NSUInteger MAX_SCANNED_PAGES = 8;
                NSUInteger scannedOcrCount = 0;
                
                NSMutableString *result = [NSMutableString string];
                
                for (NSUInteger i = 0; i < doc.pageCount; i++) {
                    // Every page leaves behind a rendered bitmap and a pile of Vision buffers,
                    // so they are drained before the next page starts
                    @autoreleasepool {
                        // The whole per-page body is guarded: a page that throws is skipped
                        // instead of costing the rest of the document
                        @try {
                            PDFPage *page = [doc pageAtIndex:i];
                            if (page) {
                                NSString *pageText = page.string;
                                BOOL hasText = pageText != nil
                                    && [pageText stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]].length > 0;
                                
                                if (hasText) {
                                    [result appendFormat:@"%@\n", pageText];
                                } else if (scannedOcrCount < MAX_SCANNED_PAGES) {
                                    CGRect pageRect = [page boundsForBox:kPDFDisplayBoxMediaBox];
                                    
                                    // 108 dpi keeps small type legible to Vision while roughly
                                    // halving inference time per page; the long side is capped so
                                    // an oversized page cannot allocate an enormous bitmap
                                    CGFloat scale = 1.5;
                                    CGFloat maxDim = MAX(pageRect.size.width, pageRect.size.height);
                                    if (maxDim * scale > 2000.0 && maxDim > 0.0) {
                                        scale = 2000.0 / maxDim;
                                    }
                                    size_t width = (size_t)(pageRect.size.width * scale);
                                    size_t height = (size_t)(pageRect.size.height * scale);
                                    
                                    if (width > 0 && height > 0) {
                                        CGColorSpaceRef colorSpace = CGColorSpaceCreateDeviceRGB();
                                        CGContextRef ctx = CGBitmapContextCreate(NULL, width, height, 8, width * 4, colorSpace,
                                                                                 kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
                                        CGColorSpaceRelease(colorSpace);
                                        
                                        if (ctx) {
                                            // White background first, otherwise transparent pixels read as noise
                                            CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);
                                            CGContextFillRect(ctx, CGRectMake(0, 0, (CGFloat)width, (CGFloat)height));
                                            
                                            CGContextSaveGState(ctx);
                                            CGContextScaleCTM(ctx, scale, scale);
                                            [page drawWithBox:kPDFDisplayBoxMediaBox toContext:ctx];
                                            CGContextRestoreGState(ctx);
                                            
                                            CGImageRef rendered = CGBitmapContextCreateImage(ctx);
                                            // The image holds its own copy of the pixels, so the
                                            // context is released the moment it is captured
                                            CGContextRelease(ctx);
                                            
                                            if (rendered) {
                                                NSString *ocrText = ocr_cgimage(rendered);
                                                if (ocrText.length > 0) {
                                                    [result appendFormat:@"%@\n", ocrText];
                                                }
                                                CGImageRelease(rendered);
                                                scannedOcrCount++;
                                            }
                                        }
                                    }
                                }
                            }
                        } @catch (NSException *e) {
                            // Skip the bad page and keep going
                        }
                    }
                }
                
                return strdup([result UTF8String]);
            }
            
            CGImageSourceRef imageSource = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
            if (!imageSource) return strdup("");
            
            CGImageRef cgImage = CGImageSourceCreateImageAtIndex(imageSource, 0, NULL);
            if (!cgImage) {
                CFRelease(imageSource);
                return strdup("");
            }
            
            NSString *result = ocr_cgimage(cgImage);
            
            CGImageRelease(cgImage);
            CFRelease(imageSource);
            
            return strdup([result UTF8String]);
        } @catch (NSException *e) {
            // A malformed or corrupted file must never abort the indexing worker
            return strdup("");
        }
    }
}

void free_ocr_string(char* str) {
    if (str) {
        free(str);
    }
}