// Test-only interposer. No IOKit implementation is EVER called. CoreFoundation
// dictionaries/run-loop identities are real, but no devices are scheduled.
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdint.h>
#include <crt_externs.h>
#define INTERPOSE(repl, orig) \
 __attribute__((used)) static struct { const void *r; const void *o; } pair_##orig \
 __attribute__((section("__DATA,__interpose"))) = {(const void *)(repl),(const void *)(orig)}
extern uint32_t IOHIDCheckAccess(uint32_t);
extern bool IOHIDRequestAccess(uint32_t);
extern void *IOHIDManagerCreate(void *, uint32_t);
extern void IOHIDManagerSetDeviceMatching(void *, void *);
extern void IOHIDManagerRegisterDeviceMatchingCallback(void *, void *, void *);
extern void IOHIDManagerRegisterDeviceRemovalCallback(void *, void *, void *);
extern void IOHIDManagerRegisterInputValueCallback(void *, void *, void *);
extern void IOHIDManagerScheduleWithRunLoop(void *, void *, void *);
extern void IOHIDManagerUnscheduleFromRunLoop(void *, void *, void *);
extern int IOHIDManagerOpen(void *, uint32_t);
extern int IOHIDManagerClose(void *, uint32_t);
extern void *IOHIDValueGetElement(void *);
extern uint32_t IOHIDElementGetUsagePage(void *);
extern uint32_t IOHIDElementGetUsage(void *);
extern void *IOHIDElementGetDevice(void *);
extern void *IOHIDDeviceGetProperty(void *, void *);
extern long IOHIDValueGetIntegerValue(void *);
extern uint64_t IOHIDValueGetTimeStamp(void *);
// Fail-closed marker for the resource child before it can call any HID API.
uint64_t rustic_keys_test_mock_marker(void) { return UINT64_C(0x524b544553544f4e); }
static char fixture(void) { FILE *f=fopen(getenv("RK_MOCK_PERMISSION"),"r"); int c=f?fgetc(f):'D';if(f)fclose(f);return c; }
static void record(const char *s) { FILE *f=fopen(getenv("RK_MOCK_LOG"),"a"); if(f){fprintf(f,"%s\n",s);fclose(f);} }
static uint32_t check(uint32_t r) {
 if (*_NSGetArgc()>1 && strcmp((*_NSGetArgv())[1],"check-access")==0) {
  record("helper check");
  if(fixture()=='H')usleep(2000000); // child must be killed/reaped by 300 ms deadline
 }
 static int cached=-1;
 if(cached<0 || !getenv("RK_MOCK_CACHE")) cached=fixture()=='G'?0:1;
 return cached;
}
static bool request(uint32_t r) {record("request mocked: NO PROMPT");return false;}
// A real owned CFArray, not an IOHIDManager. All operations that would use
// this as a manager are interposed no-ops. The actual CFRelease is safe and
// its element release callback provides a narrow cleanup observation.
static void released_marker(CFAllocatorRef a, const void *v) {record("release manager");}
static void *create(void *a,uint32_t b) {
 record("create");
 const void *marker=(void *)0x1011;
 CFArrayCallBacks cb={0,NULL,released_marker,NULL,NULL};
 return (void *)CFArrayCreate(kCFAllocatorDefault,&marker,1,&cb);
}
static void matching(void *a,void *b) {}
static void callbacks(void *a,void *b,void *c) {}
static void schedule(void *a,void *b,void *c) {record("schedule");}
static void unschedule(void *a,void *b,void *c) {record("unschedule");}
static int open_manager(void *a,uint32_t b) {record(b==0?"open listen-only":"FORBIDDEN seize");return getenv("RK_MOCK_OPEN_FAIL")?(int)0xe00002e2:0;}
static int close_manager(void *a,uint32_t b) {record("close");return 0;}
static SInt32 run_mode(CFRunLoopMode m,CFTimeInterval t,Boolean once) {
 if(getenv("RK_MOCK_RETURN_LOOP")){record("runloop returned");return kCFRunLoopRunFinished;}
 usleep(100000);return kCFRunLoopRunTimedOut;
}
static void run(void) {while(!getenv("RK_MOCK_RETURN_LOOP"))usleep(100000);}
static void *element(void *v){return (void *)0x3000;}
static uint32_t page(void *v){return 7;}
static uint32_t usage(void *v){return 4;}
static void *device(void *v){return (void *)0x4000;}
static void *property(void *a,void *b){return NULL;}
static long value(void *v){return 1;}
static uint64_t timestamp(void *v){return 0;}
INTERPOSE(check,IOHIDCheckAccess);INTERPOSE(request,IOHIDRequestAccess);
INTERPOSE(create,IOHIDManagerCreate);INTERPOSE(matching,IOHIDManagerSetDeviceMatching);
INTERPOSE(callbacks,IOHIDManagerRegisterDeviceMatchingCallback);INTERPOSE(callbacks,IOHIDManagerRegisterDeviceRemovalCallback);INTERPOSE(callbacks,IOHIDManagerRegisterInputValueCallback);
INTERPOSE(schedule,IOHIDManagerScheduleWithRunLoop);INTERPOSE(unschedule,IOHIDManagerUnscheduleFromRunLoop);
INTERPOSE(open_manager,IOHIDManagerOpen);INTERPOSE(close_manager,IOHIDManagerClose);
INTERPOSE(run_mode,CFRunLoopRunInMode);INTERPOSE(run,CFRunLoopRun);
INTERPOSE(element,IOHIDValueGetElement);INTERPOSE(page,IOHIDElementGetUsagePage);INTERPOSE(usage,IOHIDElementGetUsage);INTERPOSE(device,IOHIDElementGetDevice);INTERPOSE(property,IOHIDDeviceGetProperty);INTERPOSE(value,IOHIDValueGetIntegerValue);INTERPOSE(timestamp,IOHIDValueGetTimeStamp);
